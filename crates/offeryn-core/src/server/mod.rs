use crate::McpError;
use jsonrpc_core::{
    Call, ErrorCode, Failure, Output, Params, Request as JsonRpcRequest,
    Response as JsonRpcResponse, Success, Version,
};
use offeryn_types::*;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

pub struct McpServer {
    name: String,
    version: String,
    tools: Mutex<HashMap<String, Box<dyn McpTool>>>,
}

impl McpServer {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            tools: Mutex::new(HashMap::new()),
        }
    }

    pub async fn with_tool(&self, tool: impl McpTool + 'static) -> &Self {
        let tool_name = tool.name().to_string();
        info!(tool_name = %tool_name, "Registering tool");
        self.tools.lock().await.insert(tool_name, Box::new(tool));
        self
    }

    pub async fn with_tools(&self, tools: Vec<Box<dyn McpTool>>) -> &Self {
        let mut tools_lock = self.tools.lock().await;
        for tool in tools {
            let name = tool.name().to_string();
            info!(tool_name = %name, "Registering tool");
            tools_lock.insert(name, tool);
        }
        self
    }

    pub async fn register_tool<T: McpTool + 'static>(&self, tool: T) {
        let tool_name = tool.name().to_string();
        info!(tool_name = %tool_name, "Registering tool");
        self.tools.lock().await.insert(tool_name, Box::new(tool));
    }

    pub async fn register_tools<T: HasTools>(&self, provider: T)
    where
        T::Tools: IntoIterator<Item = Box<dyn McpTool>>,
    {
        let mut tools_lock = self.tools.lock().await;
        for tool in provider.tools() {
            let name = tool.name().to_string();
            info!(tool_name = %name, "Registering tool");
            tools_lock.insert(name, tool);
        }
    }

    pub async fn handle_request(
        &self,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, McpError> {
        let (id, method, params) = match request {
            JsonRpcRequest::Single(Call::MethodCall(call)) => {
                debug!(
                    method = %call.method,
                    id = ?call.id,
                    params = %serde_json::to_string_pretty(&call.params).unwrap_or_default(),
                    "Received JSON-RPC request"
                );
                (call.id, call.method, call.params)
            }
            JsonRpcRequest::Single(Call::Notification(notification)) => {
                debug!(
                    method = %notification.method,
                    params = %serde_json::to_string_pretty(&notification.params).unwrap_or_default(),
                    "Received JSON-RPC notification"
                );
                // For now, just return an empty success response
                // TODO
                return Ok(JsonRpcResponse::Single(Output::Success(Success {
                    jsonrpc: Some(Version::V2),
                    result: serde_json::json!({}),
                    id: Id::Num(0),
                })));
            }
            _ => {
                return Ok(JsonRpcResponse::Single(Output::Failure(Failure {
                    jsonrpc: Some(Version::V2),
                    error: McpError::InvalidRequest.into(),
                    id: Id::Num(0),
                })));
            }
        };

        let response = match method.as_str() {
            "initialize" => {
                info!("Processing initialize request");
                let tools_lock = self.tools.lock().await;
                let capabilities = ServerCapabilities {
                    tools: tools_lock.keys().map(|k| (k.clone(), true)).collect(),
                };

                let result = InitializeResult {
                    protocol_version: LATEST_PROTOCOL_VERSION.to_string(),
                    capabilities,
                    server_info: ServerInfo {
                        name: self.name.clone(),
                        version: self.version.clone(),
                    },
                    instructions: Some("Use tools/list to see available tools".to_string()),
                };

                debug!(
                    server_name = %self.name,
                    server_version = %self.version,
                    protocol_version = %LATEST_PROTOCOL_VERSION,
                    num_tools = %tools_lock.len(),
                    "Sending initialize response"
                );

                JsonRpcResponse::Single(Output::Success(Success {
                    jsonrpc: Some(Version::V2),
                    result: serde_json::to_value(result)?,
                    id,
                }))
            }
            "tools/list" => {
                info!("Processing tools/list request");
                let tools_lock = self.tools.lock().await;
                let tools: Vec<Tool> = tools_lock
                    .values()
                    .map(|tool| Tool {
                        name: tool.name().to_string(),
                        description: tool.description().to_string(),
                        input_schema: tool.input_schema(),
                    })
                    .collect();

                let result = ListToolsResult {
                    tools,
                    next_page_token: None, // Pagination not implemented yet
                };

                debug!(
                    num_tools = %result.tools.len(),
                    tool_names = ?result.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                    "Sending tools list response"
                );

                JsonRpcResponse::Single(Output::Success(Success {
                    jsonrpc: Some(Version::V2),
                    result: serde_json::to_value(result)?,
                    id,
                }))
            }
            "tools/call" => {
                info!("Processing tools/call request");
                let params = match params {
                    Params::Map(map) => map,
                    _ => {
                        warn!("Invalid params format for tools/call - expected Map");
                        return Err(McpError::InvalidParams);
                    }
                };

                let request: CallToolRequest =
                    serde_json::from_value(serde_json::Value::Object(params)).map_err(|_| {
                        warn!("Failed to parse tool call request parameters");
                        McpError::InvalidParams
                    })?;

                debug!(
                    tool = %request.name,
                    args = ?request.arguments,
                    "Executing tool"
                );

                let tools_lock = self.tools.lock().await;
                let tool = tools_lock.get(&request.name).ok_or_else(|| {
                    warn!(tool = %request.name, "Tool not found");
                    McpError::MethodNotFound
                })?;

                let args = match request.arguments {
                    Some(args) => serde_json::Value::Object(args.into_iter().collect()),
                    None => serde_json::json!({}),
                };

                debug!(
                    tool = %request.name,
                    args = %serde_json::to_string_pretty(&args).unwrap_or_default(),
                    "Executing tool with arguments"
                );

                match tool.execute(args).await {
                    Ok(result) => {
                        let content = result
                            .content
                            .into_iter()
                            .map(|c| Content::Text { text: c.text })
                            .collect();

                        let result = CallToolResult {
                            content,
                            is_error: Some(result.is_error),
                        };

                        debug!(
                            tool = %request.name,
                            is_error = ?result.is_error,
                            content_length = %result.content.len(),
                            "Tool execution successful"
                        );

                        JsonRpcResponse::Single(Output::Success(Success {
                            jsonrpc: Some(Version::V2),
                            result: serde_json::to_value(result)?,
                            id,
                        }))
                    }
                    Err(e) => {
                        warn!(
                            tool = %request.name,
                            error = %e,
                            "Tool execution failed"
                        );
                        JsonRpcResponse::Single(Output::Failure(Failure {
                            jsonrpc: Some(Version::V2),
                            error: JsonRpcError::new(ErrorCode::ServerError(-32000)),
                            id,
                        }))
                    }
                }
            }
            _ => {
                warn!(method = %method, "Unknown method called");
                JsonRpcResponse::Single(Output::Failure(Failure {
                    jsonrpc: Some(Version::V2),
                    error: JsonRpcError::method_not_found(),
                    id,
                }))
            }
        };

        // Log the full JSON response
        info!(
            method = %method,
            response = %serde_json::to_string_pretty(&response).unwrap_or_default(),
            "Full JSON response"
        );

        Ok(response)
    }

    pub fn handle_notification(
        &mut self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        match method {
            "notifications/initialized" => {
                info!("Client completed initialization");
                Ok(())
            }
            _ => Err(McpError::MethodNotFound),
        }
    }
}
