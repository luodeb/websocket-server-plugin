use futures_util::{SinkExt, StreamExt};
use plugin_interfaces::{
    create_plugin_interface_from_handler, log_info, log_warn,
    pluginui::{Context, Ui},
    PluginHandler, PluginInstanceContext, PluginInterface,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{runtime::Runtime, sync::Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use uuid::Uuid;

/// WebSocket 客户端信息
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub id: String,
    pub addr: SocketAddr,
    pub sender: Arc<
        Mutex<
            futures_util::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
                Message,
            >,
        >,
    >,
}

/// WebSocket 服务器插件实现
#[derive(Clone)]
pub struct WebSocketServerPlugin {
    server_running: Arc<Mutex<bool>>,
    server_address: String,
    server_port: String,
    clients: Arc<Mutex<HashMap<String, ClientInfo>>>,
    selected_client: Option<String>,
    runtime: Option<Arc<Runtime>>,
    server_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl WebSocketServerPlugin {
    fn new() -> Self {
        Self {
            server_running: Arc::new(Mutex::new(false)),
            server_address: "127.0.0.1".to_string(),
            server_port: "8080".to_string(),
            clients: Arc::new(Mutex::new(HashMap::new())),
            selected_client: None,
            runtime: None,
            server_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 启动 WebSocket 服务器
    async fn start_server(&self, plugin_ctx: PluginInstanceContext) {
        let addr = format!("{}:{}", self.server_address, self.server_port);
        log_info!("Starting WebSocket server on {}", addr);

        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                *self.server_running.lock().await = true;
                plugin_ctx.send_message_to_frontend(&format!(
                    "WebSocket 服务器已启动，监听地址: {}",
                    addr
                ));
                plugin_ctx.refresh_ui();

                let server_running = self.server_running.clone();
                let plugin_ctx_clone = plugin_ctx.clone();

                // 接受连接的主循环
                loop {
                    // 检查是否应该停止服务器
                    if !*server_running.lock().await {
                        log_info!("Server stop flag detected, breaking accept loop");
                        break;
                    }

                    // 使用 select! 来同时监听连接和停止信号
                    tokio::select! {
                        result = listener.accept() => {
                            match result {
                                Ok((stream, addr)) => {
                                    log_info!("New connection from: {}", addr);

                                    let clients_clone = self.clients.clone();
                                    let plugin_ctx_clone2 = plugin_ctx_clone.clone();

                                    tokio::spawn(async move {
                                        Self::handle_client(stream, addr, clients_clone, plugin_ctx_clone2)
                                            .await;
                                    });
                                }
                                Err(e) => {
                                    log_warn!("Failed to accept connection: {}", e);
                                    // 如果接受连接失败，可能是服务器被关闭了
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                            // 定期检查停止标志，确保能及时响应停止请求
                            continue;
                        }
                    }
                }

                log_info!("WebSocket server accept loop ended");
                plugin_ctx_clone.send_message_to_frontend("WebSocket 服务器接受循环已结束");
            }
            Err(e) => {
                log_warn!("Failed to bind to {}: {}", addr, e);
                plugin_ctx.send_message_to_frontend(&format!("启动服务器失败: {}", e));
            }
        }
    }

    /// 处理单个客户端连接
    async fn handle_client(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        clients: Arc<Mutex<HashMap<String, ClientInfo>>>,
        plugin_ctx: PluginInstanceContext,
    ) {
        match accept_async(stream).await {
            Ok(ws_stream) => {
                let client_id = Uuid::new_v4().to_string();
                log_info!(
                    "WebSocket connection established with client: {} ({})",
                    client_id,
                    addr
                );

                let (ws_sender, mut ws_receiver) = ws_stream.split();

                // 创建客户端信息
                let client_info = ClientInfo {
                    id: client_id.clone(),
                    addr,
                    sender: Arc::new(Mutex::new(ws_sender)),
                };

                // 添加到客户端列表
                clients.lock().await.insert(client_id.clone(), client_info);
                plugin_ctx
                    .send_message_to_frontend(&format!("客户端已连接: {} ({})", client_id, addr));
                plugin_ctx.refresh_ui();

                // 处理消息接收
                while let Some(msg) = ws_receiver.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            log_info!("Received message from {}: {}", client_id, text);
                            plugin_ctx
                                .send_message_to_frontend(&format!("[{}] {}", client_id, text));
                        }
                        Ok(Message::Close(_)) => {
                            log_info!("Client {} disconnected", client_id);
                            break;
                        }
                        Err(e) => {
                            log_warn!("WebSocket error for client {}: {}", client_id, e);
                            break;
                        }
                        _ => {}
                    }
                }

                // 移除客户端
                clients.lock().await.remove(&client_id);
                plugin_ctx
                    .send_message_to_frontend(&format!("客户端已断开: {} ({})", client_id, addr));
                plugin_ctx.refresh_ui();
            }
            Err(e) => {
                log_warn!("WebSocket handshake failed for {}: {}", addr, e);
            }
        }
    }

    /// 停止 WebSocket 服务器
    async fn stop_server(&self, plugin_ctx: &PluginInstanceContext) {
        log_info!("Stopping WebSocket server...");

        // 1. 首先设置停止标志
        *self.server_running.lock().await = false;

        // 2. 取消服务器任务
        if let Some(handle) = self.server_handle.lock().await.take() {
            log_info!("Aborting server task...");
            handle.abort();

            // 等待任务完全结束
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // 3. 断开所有客户端连接
        {
            let mut clients = self.clients.lock().await;
            log_info!("Disconnecting {} clients...", clients.len());

            // 向所有客户端发送关闭消息
            for (client_id, client) in clients.iter() {
                if let Ok(mut sender) = client.sender.try_lock() {
                    let _ = sender.send(Message::Close(None)).await;
                    log_info!("Sent close message to client: {}", client_id);
                }
            }

            // 清空客户端列表
            clients.clear();
        }

        // 4. 等待一小段时间确保所有连接都已断开
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // 5. 通知前端和刷新UI
        plugin_ctx.send_message_to_frontend("WebSocket 服务器已完全停止");
        plugin_ctx.refresh_ui();

        log_info!("WebSocket server completely stopped");
    }

    /// 发送消息到指定客户端
    async fn send_message_to_client(&self, client_id: &str, message: &str) -> Result<(), String> {
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(client_id) {
            let mut sender = client.sender.lock().await;
            match sender.send(Message::Text(message.to_string())).await {
                Ok(_) => {
                    log_info!("Message sent to client {}: {}", client_id, message);
                    Ok(())
                }
                Err(e) => {
                    log_warn!("Failed to send message to client {}: {}", client_id, e);
                    Err(format!("发送失败: {}", e))
                }
            }
        } else {
            Err("客户端不存在".to_string())
        }
    }

    /// 广播消息到所有客户端
    async fn broadcast_message(&self, message: &str) -> Result<(), String> {
        let clients = self.clients.lock().await;
        let mut errors = Vec::new();

        for (client_id, client) in clients.iter() {
            let mut sender = client.sender.lock().await;
            if let Err(e) = sender.send(Message::Text(message.to_string())).await {
                errors.push(format!("客户端 {}: {}", client_id, e));
            }
        }

        if errors.is_empty() {
            log_info!(
                "Message broadcasted to {} clients: {}",
                clients.len(),
                message
            );
            Ok(())
        } else {
            Err(format!("部分发送失败: {}", errors.join(", ")))
        }
    }

    /// 启动服务器的异步任务
    fn start_server_task(&self, plugin_ctx: PluginInstanceContext) {
        if let Some(runtime) = &self.runtime {
            let self_clone = self.clone();
            let handle = runtime.spawn(async move {
                self_clone.start_server(plugin_ctx).await;
            });

            if let Ok(mut server_handle) = self.server_handle.try_lock() {
                *server_handle = Some(handle);
            }
        }
    }
}

impl PluginHandler for WebSocketServerPlugin {
    fn update_ui(&mut self, _ctx: &Context, ui: &mut Ui, _plugin_ctx: &PluginInstanceContext) {
        ui.label("WebSocket 服务器插件");

        // 服务器控制区域
        ui.horizontal(|ui| {
            ui.label("服务器地址:");
            let text_response = ui.text_edit_singleline(&mut self.server_address);
            if text_response.changed() {
                log_info!("Server address changed to: {}", self.server_address);
            }
        });
        ui.horizontal(|ui| {
            ui.label("服务器端口:");
            let port_response = ui.text_edit_singleline(&mut self.server_port);
            if port_response.changed() {
                log_info!("Server port changed to: {}", self.server_port);
            }
        });

        ui.label(""); // 空行

        // 客户端选择区域
        ui.horizontal(|ui| {
            ui.label("选择客户端:");

            // 获取客户端列表
            let mut client_options = vec!["全局广播".to_string()];
            if let Ok(clients) = self.clients.try_lock() {
                for (client_id, _) in clients.iter() {
                    client_options.push(client_id.to_string());
                }
            }

            let combo_response =
                ui.combo_box(client_options, &mut self.selected_client, "选择目标客户端");
            if combo_response.clicked() {
                log_info!("Selected client: {:?}", self.selected_client);
            }
        });
    }

    fn on_mount(
        &mut self,
        plugin_ctx: &PluginInstanceContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = plugin_ctx.get_metadata();
        log_info!(
            "[{}] WebSocket Server Plugin mounted successfully",
            metadata.name
        );

        // 初始化 tokio 异步运行时
        match Runtime::new() {
            Ok(runtime) => {
                self.runtime = Some(Arc::new(runtime));
                log_info!("Tokio runtime initialized successfully");
            }
            Err(e) => {
                log_warn!("Failed to initialize tokio runtime: {}", e);
            }
        }

        Ok(())
    }

    fn on_dispose(
        &mut self,
        plugin_ctx: &PluginInstanceContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = plugin_ctx.get_metadata();
        log_info!("[{}] WebSocket Server Plugin disposing", metadata.name);

        // 停止服务器
        if let Some(runtime) = &self.runtime {
            let self_clone = self.clone();
            let plugin_ctx_clone = plugin_ctx.clone();
            runtime.spawn(async move {
                self_clone.stop_server(&plugin_ctx_clone).await;
            });
        }

        // 关闭 to异步运行时
        if let Some(runtime) = self.runtime.clone() {
            match Arc::try_unwrap(runtime) {
                Ok(runtime) => {
                    runtime.shutdown_timeout(std::time::Duration::from_millis(1000));
                    log_info!("Tokio runtime shutdown successfully");
                }
                Err(_) => {
                    log_warn!("Cannot shutdown runtime: other references still exist");
                }
            }
        }

        plugin_ctx.send_message_to_frontend("WebSocket 服务器插件已卸载");
        Ok(())
    }

    fn on_connect(
        &mut self,
        plugin_ctx: &PluginInstanceContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = plugin_ctx.get_metadata();
        log_info!("[{}] WebSocket Server Plugin connected", metadata.name);
        self.start_server_task(plugin_ctx.clone());

        Ok(())
    }

    fn on_disconnect(
        &mut self,
        plugin_ctx: &PluginInstanceContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = plugin_ctx.get_metadata();
        log_info!("[{}] WebSocket Server Plugin disconnecting", metadata.name);
        log_info!("Initiating complete server shutdown");

        if let Some(runtime) = &self.runtime {
            let self_clone = self.clone();
            let plugin_ctx_clone = plugin_ctx.clone();

            // 使用阻塞的方式等待服务器完全停止
            let handle = runtime.spawn(async move {
                self_clone.stop_server(&plugin_ctx_clone).await;
            });

            // 阻塞等待停止完成，但设置超时以防止无限等待
            match runtime.block_on(async {
                tokio::time::timeout(tokio::time::Duration::from_secs(5), handle).await
            }) {
                Ok(Ok(())) => {
                    log_info!("Server shutdown completed successfully");
                }
                Ok(Err(e)) => {
                    log_warn!("Server shutdown task failed: {:?}", e);
                }
                Err(_) => {
                    log_warn!("Server shutdown timed out after 5 seconds");
                }
            }
        } else {
            log_warn!("Runtime not available, cannot stop server properly");
        }

        log_info!("[{}] WebSocket Server Plugin disconnected", metadata.name);
        Ok(())
    }

    fn handle_message(
        &mut self,
        message: &str,
        plugin_ctx: &PluginInstanceContext,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let metadata = plugin_ctx.get_metadata();
        log_info!("[{}] Received message: {}", metadata.name, message);
        if let Some(runtime) = &self.runtime {
            let self_clone = self.clone();
            let selected_client = self.selected_client.clone();
            let message_owned = message.to_string();
            let plugin_ctx_clone = plugin_ctx.clone();

            runtime.spawn(async move {
                let result = if let Some(client_id) = selected_client {
                    self_clone
                        .send_message_to_client(&client_id, &message_owned)
                        .await
                } else {
                    self_clone.broadcast_message(&message_owned).await
                };

                match result {
                    Err(e) => {
                        plugin_ctx_clone.send_message_to_frontend(&format!("发送消息失败: {}", e));
                    }
                    Ok(_) => {
                        log_info!("Message sent successfully");
                    }
                }
            });
        }
        Ok("".to_string())
    }
}

/// 创建插件实例的导出函数
#[no_mangle]
pub extern "C" fn create_plugin() -> *mut PluginInterface {
    let plugin = WebSocketServerPlugin::new();
    let handler: Box<dyn PluginHandler> = Box::new(plugin);
    create_plugin_interface_from_handler(handler)
}
/// 销毁插件实例的导出函数
///
/// # Safety
///
/// This function is unsafe because it dereferences raw pointers.
/// The caller must ensure that:
/// - `interface` is a valid pointer to a `PluginInterface` that was created by `create_plugin`
/// - `interface` has not been freed or destroyed previously
/// - The `PluginInterface` and its associated plugin instance are in a valid state
#[no_mangle]
pub unsafe extern "C" fn destroy_plugin(interface: *mut PluginInterface) {
    if !interface.is_null() {
        ((*interface).destroy)((*interface).plugin_ptr);
        let _ = Box::from_raw(interface);
    }
}
