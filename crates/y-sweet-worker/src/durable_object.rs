use crate::{
    config::Configuration, server_context::ServerContext, threadless::Threadless, DocIdPair,
};
use futures::StreamExt;
use std::sync::Arc;
use worker::{
    durable_object, Env, Request, Response, Result, RouteContext, Router, State, WebSocketPair,
};
#[allow(unused)]
use worker_sys::console_log;
use y_sweet_core::{doc_connection::DocConnection, doc_sync::DocWithSyncKv};

#[durable_object]
pub struct YServe {
    env: Env,
    lazy_doc: Option<DocIdPair>,
    state: State,
}

impl YServe {
    /// We need to lazily create the doc because the constructor is non-async.
    pub async fn get_doc(&mut self, req: &Request, doc_id: &str) -> Result<&mut DocWithSyncKv> {
        if self.lazy_doc.is_none() {
            let mut context = ServerContext::from_request(req, &self.env).unwrap();
            #[allow(clippy::arc_with_non_send_sync)] // Arc required for compatibility with core.
            let storage = Arc::new(self.state.storage());

            let store = Some(context.store());
            let storage = Threadless(storage);
            let config = Configuration::try_from(&self.env).map_err(|e| e.to_string())?;
            let timeout_interval_ms: i64 = config
                .timeout_interval
                .as_millis()
                .try_into()
                .expect("Should be able to convert timeout interval to i64");

            let doc = DocWithSyncKv::new(doc_id, store, move || {
                let storage = storage.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    console_log!("Setting alarm.");
                    storage.0.set_alarm(timeout_interval_ms).await.unwrap();
                });
            })
            .await
            .unwrap();

            self.lazy_doc = Some(DocIdPair {
                doc,
                id: doc_id.to_owned(),
            });
            self.lazy_doc
                .as_mut()
                .unwrap()
                .doc
                .sync_kv()
                .persist()
                .await
                .unwrap();
        }

        Ok(&mut self.lazy_doc.as_mut().unwrap().doc)
    }
}

#[durable_object]
impl DurableObject for YServe {
    fn new(state: State, env: Env) -> Self {
        Self {
            env,
            state,
            lazy_doc: None,
        }
    }

    async fn fetch(&mut self, req: Request) -> Result<Response> {
        let env: Env = self.env.clone().into();
        let req = ServerContext::reconstruct_request(&req)?;

        Router::with_data(self)
            .get_async("/doc/ws/:doc_id", websocket_connect)
            .run(req, env)
            .await
    }

    async fn alarm(&mut self) -> Result<Response> {
        console_log!("Alarm!");
        let DocIdPair { id, doc } = self.lazy_doc.as_ref().unwrap();
        doc.sync_kv().persist().await.unwrap();
        console_log!("Persisted. {}", id);
        Response::ok("ok")
    }
}

async fn websocket_connect(req: Request, ctx: RouteContext<&mut YServe>) -> Result<Response> {
    let WebSocketPair { client, server } = WebSocketPair::new()?;
    server.accept()?;

    let doc_id = ctx.param("doc_id").unwrap().to_owned();
    let awareness = ctx.data.get_doc(&req, &doc_id).await.unwrap().awareness();

    let connection = {
        let server = server.clone();
        DocConnection::new(awareness, move |bytes| {
            let result = server.send_with_bytes(bytes);
            if let Err(result) = result {
                console_log!("Error sending bytes: {:?}", result);
            }
        })
    };

    wasm_bindgen_futures::spawn_local(async move {
        let mut events = server.events().unwrap();

        while let Some(event) = events.next().await {
            match event.unwrap() {
                worker::WebsocketEvent::Message(message) => {
                    if let Some(bytes) = message.bytes() {
                        let result = connection.send(&bytes).await;
                        if let Err(result) = result {
                            console_log!("Error sending bytes: {:?}", result);
                        }
                    } else {
                        server
                            .send_with_str("Received unexpected text message.")
                            .unwrap()
                    }
                }
                worker::WebsocketEvent::Close(_) => {
                    let _ = server.close::<&str>(None, None);
                    break;
                }
            }
        }
    });

    let resp = Response::from_websocket(client)?;
    Ok(resp)
}
