use crate::services::{
    doc::{web_socket::web_socket::EditorWebSocket, SYNC_INTERVAL_IN_MILLIS},
    ws_handlers::{DocumentWebSocket, DocumentWsHandler},
};
use async_stream::stream;
use bytes::Bytes;
use flowy_collaboration::entities::ws::{DocumentWSData, DocumentWSDataType, NewDocumentUser};
use flowy_error::{internal_error, FlowyError, FlowyResult};
use futures::stream::StreamExt;
use lib_infra::future::FutureResult;
use lib_ot::revision::RevisionRange;
use lib_ws::WSConnectState;
use std::{convert::TryFrom, sync::Arc};
use tokio::{
    sync::{
        broadcast,
        mpsc,
        mpsc::{UnboundedReceiver, UnboundedSender},
    },
    task::spawn_blocking,
    time::{interval, Duration},
};

pub struct EditorHttpWebSocket {
    doc_id: String,
    data_provider: Arc<dyn DocumentWSSinkDataProvider>,
    stream_consumer: Arc<dyn DocumentWSSteamConsumer>,
    ws: Arc<dyn DocumentWebSocket>,
    ws_msg_tx: UnboundedSender<DocumentWSData>,
    ws_msg_rx: Option<UnboundedReceiver<DocumentWSData>>,
    stop_sync_tx: SinkStopTx,
    state: broadcast::Sender<WSConnectState>,
}

impl EditorHttpWebSocket {
    pub fn new(
        doc_id: &str,
        ws: Arc<dyn DocumentWebSocket>,
        data_provider: Arc<dyn DocumentWSSinkDataProvider>,
        stream_consumer: Arc<dyn DocumentWSSteamConsumer>,
    ) -> Self {
        let (ws_msg_tx, ws_msg_rx) = mpsc::unbounded_channel();
        let (stop_sync_tx, _) = tokio::sync::broadcast::channel(2);
        let doc_id = doc_id.to_string();
        let (state, _) = broadcast::channel(2);
        let mut manager = EditorHttpWebSocket {
            doc_id,
            data_provider,
            stream_consumer,
            ws,
            ws_msg_tx,
            ws_msg_rx: Some(ws_msg_rx),
            stop_sync_tx,
            state,
        };
        manager.start_web_socket();
        manager
    }

    fn start_web_socket(&mut self) {
        let ws_msg_rx = self.ws_msg_rx.take().expect("Only take once");
        let sink = DocumentWebSocketSink::new(
            &self.doc_id,
            self.data_provider.clone(),
            self.ws.clone(),
            self.stop_sync_tx.subscribe(),
        );
        let stream = DocumentWebSocketStream::new(
            &self.doc_id,
            self.stream_consumer.clone(),
            ws_msg_rx,
            self.stop_sync_tx.subscribe(),
        );
        tokio::spawn(sink.run());
        tokio::spawn(stream.run());
    }

    pub fn scribe_state(&self) -> broadcast::Receiver<WSConnectState> { self.state.subscribe() }
}

impl EditorWebSocket for Arc<EditorHttpWebSocket> {
    fn stop_web_socket(&self) {
        if self.stop_sync_tx.send(()).is_ok() {
            tracing::debug!("{} stop sync", self.doc_id)
        }
    }

    fn ws_handler(&self) -> Arc<dyn DocumentWsHandler> { self.clone() }
}

impl DocumentWsHandler for EditorHttpWebSocket {
    fn receive(&self, doc_data: DocumentWSData) {
        match self.ws_msg_tx.send(doc_data) {
            Ok(_) => {},
            Err(e) => tracing::error!("❌Propagate ws message failed. {}", e),
        }
    }

    fn connect_state_changed(&self, state: &WSConnectState) {
        match self.state.send(state.clone()) {
            Ok(_) => {},
            Err(e) => tracing::error!("{}", e),
        }
    }
}

pub trait DocumentWSSteamConsumer: Send + Sync {
    fn receive_push_revision(&self, bytes: Bytes) -> FutureResult<(), FlowyError>;
    fn receive_ack(&self, id: String, ty: DocumentWSDataType) -> FutureResult<(), FlowyError>;
    fn receive_new_user_connect(&self, new_user: NewDocumentUser) -> FutureResult<(), FlowyError>;
    fn send_revision_in_range(&self, range: RevisionRange) -> FutureResult<(), FlowyError>;
}

pub struct DocumentWebSocketStream {
    doc_id: String,
    consumer: Arc<dyn DocumentWSSteamConsumer>,
    ws_msg_rx: Option<mpsc::UnboundedReceiver<DocumentWSData>>,
    stop_rx: Option<SinkStopRx>,
}

impl DocumentWebSocketStream {
    pub fn new(
        doc_id: &str,
        consumer: Arc<dyn DocumentWSSteamConsumer>,
        ws_msg_rx: mpsc::UnboundedReceiver<DocumentWSData>,
        stop_rx: SinkStopRx,
    ) -> Self {
        DocumentWebSocketStream {
            doc_id: doc_id.to_owned(),
            consumer,
            ws_msg_rx: Some(ws_msg_rx),
            stop_rx: Some(stop_rx),
        }
    }

    pub async fn run(mut self) {
        let mut receiver = self.ws_msg_rx.take().expect("Only take once");
        let mut stop_rx = self.stop_rx.take().expect("Only take once");
        let doc_id = self.doc_id.clone();
        let stream = stream! {
            loop {
                tokio::select! {
                    result = receiver.recv() => {
                        match result {
                            Some(msg) => {
                                yield msg
                            },
                            None => {
                                tracing::debug!("[DocumentStream:{}] loop exit", doc_id);
                                break;
                            },
                        }
                    },
                    _ = stop_rx.recv() => {
                        tracing::debug!("[DocumentStream:{}] loop exit", doc_id);
                        break
                    },
                };
            }
        };

        stream
            .for_each(|msg| async {
                match self.handle_message(msg).await {
                    Ok(_) => {},
                    Err(e) => log::error!("[DocumentStream:{}] error: {}", self.doc_id, e),
                }
            })
            .await;
    }

    async fn handle_message(&self, msg: DocumentWSData) -> FlowyResult<()> {
        let DocumentWSData {
            doc_id: _,
            ty,
            data,
            id,
        } = msg;
        let bytes = spawn_blocking(move || Bytes::from(data))
            .await
            .map_err(internal_error)?;

        tracing::debug!("[DocumentStream]: receives new message: {:?}", ty);
        match ty {
            DocumentWSDataType::PushRev => {
                let _ = self.consumer.receive_push_revision(bytes).await?;
                let _ = self.consumer.receive_ack(id, ty).await;
            },
            DocumentWSDataType::PullRev => {
                let range = RevisionRange::try_from(bytes)?;
                let _ = self.consumer.send_revision_in_range(range).await?;
            },
            DocumentWSDataType::Ack => {
                let _ = self.consumer.receive_ack(id, ty).await;
            },
            DocumentWSDataType::UserConnect => {
                let new_user = NewDocumentUser::try_from(bytes)?;
                let _ = self.consumer.receive_new_user_connect(new_user).await;
                // Notify the user that someone has connected to this document
            },
        }

        Ok(())
    }
}

pub type Tick = ();
pub type SinkStopRx = broadcast::Receiver<()>;
pub type SinkStopTx = broadcast::Sender<()>;

pub trait DocumentWSSinkDataProvider: Send + Sync {
    fn next(&self) -> FutureResult<Option<DocumentWSData>, FlowyError>;
}

pub struct DocumentWebSocketSink {
    provider: Arc<dyn DocumentWSSinkDataProvider>,
    ws_sender: Arc<dyn DocumentWebSocket>,
    stop_rx: Option<SinkStopRx>,
    doc_id: String,
}

impl DocumentWebSocketSink {
    pub fn new(
        doc_id: &str,
        provider: Arc<dyn DocumentWSSinkDataProvider>,
        ws_sender: Arc<dyn DocumentWebSocket>,
        stop_rx: SinkStopRx,
    ) -> Self {
        Self {
            provider,
            ws_sender,
            stop_rx: Some(stop_rx),
            doc_id: doc_id.to_owned(),
        }
    }

    pub async fn run(mut self) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut stop_rx = self.stop_rx.take().expect("Only take once");
        let doc_id = self.doc_id.clone();
        tokio::spawn(tick(tx));
        let stream = stream! {
            loop {
                tokio::select! {
                    result = rx.recv() => {
                        match result {
                            Some(msg) => yield msg,
                            None => break,
                        }
                    },
                    _ = stop_rx.recv() => {
                        tracing::debug!("[DocumentSink:{}] loop exit", doc_id);
                        break
                    },
                };
            }
        };
        stream
            .for_each(|_| async {
                match self.send_next_revision().await {
                    Ok(_) => {},
                    Err(e) => log::error!("[DocumentSink]: send msg failed, {:?}", e),
                }
            })
            .await;
    }

    async fn send_next_revision(&self) -> FlowyResult<()> {
        match self.provider.next().await? {
            None => {
                tracing::trace!("Finish synchronizing revisions");
                Ok(())
            },
            Some(data) => {
                self.ws_sender.send(data).map_err(internal_error)
                // let _ = tokio::time::timeout(Duration::from_millis(2000),
            },
        }
    }
}

async fn tick(sender: mpsc::UnboundedSender<Tick>) {
    let mut interval = interval(Duration::from_millis(SYNC_INTERVAL_IN_MILLIS));
    while sender.send(()).is_ok() {
        interval.tick().await;
    }
}
