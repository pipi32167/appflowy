use crate::{
    core::{
        document::Document,
        sync::{RevisionSynchronizer, RevisionUser},
    },
    entities::doc::Doc,
    errors::{internal_error, CollaborateError, CollaborateResult},
};
use async_stream::stream;
use dashmap::DashMap;
use futures::stream::StreamExt;
use lib_ot::{errors::OTError, revision::Revision, rich_text::RichTextDelta};
use std::sync::{
    atomic::{AtomicI64, Ordering::SeqCst},
    Arc,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::spawn_blocking,
};

pub trait ServerDocPersistence: Send + Sync {
    fn create_doc(&self, doc_id: &str, delta: RichTextDelta) -> CollaborateResult<()>;
    fn update_doc(&self, doc_id: &str, delta: RichTextDelta) -> CollaborateResult<()>;
    fn read_doc(&self, doc_id: &str) -> CollaborateResult<Doc>;
}

#[rustfmt::skip]
//  ┌─────────────────┐
//  │ServerDocManager │
//  └─────────────────┘
//           │ 1
//           ▼ n
//   ┌───────────────┐
//   │ OpenDocHandle │
//   └───────────────┘
//           │
//           ▼
// ┌──────────────────┐
// │ DocCommandQueue  │
// └──────────────────┘
//           │                   ┌──────────────────────┐     ┌────────────┐
//           ▼             ┌────▶│ RevisionSynchronizer │────▶│  Document  │
//  ┌────────────────┐     │     └──────────────────────┘     └────────────┘
//  │ServerDocEditor │─────┤
//  └────────────────┘     │     ┌────────┐       ┌────────────┐
//                         └────▶│ Users  │◆──────│RevisionUser│
//                               └────────┘       └────────────┘
pub struct ServerDocManager {
    open_doc_map: DashMap<String, Arc<OpenDocHandle>>,
    persistence: Arc<dyn ServerDocPersistence>,
}

impl ServerDocManager {
    pub fn new(persistence: Arc<dyn ServerDocPersistence>) -> Self {
        Self {
            open_doc_map: DashMap::new(),
            persistence,
        }
    }

    pub fn get(&self, doc_id: &str) -> Option<Arc<OpenDocHandle>> {
        self.open_doc_map.get(doc_id).map(|ctx| ctx.clone())
    }

    pub async fn cache(&self, doc: Doc) -> Result<(), CollaborateError> {
        let doc_id = doc.id.clone();
        let handle = spawn_blocking(|| OpenDocHandle::new(doc))
            .await
            .map_err(internal_error)?;
        let handle = Arc::new(handle?);
        self.open_doc_map.insert(doc_id, handle);
        Ok(())
    }
}

pub struct OpenDocHandle {
    sender: mpsc::Sender<DocCommand>,
}

impl OpenDocHandle {
    pub fn new(doc: Doc) -> Result<Self, CollaborateError> {
        let (sender, receiver) = mpsc::channel(100);
        let queue = DocCommandQueue::new(receiver, doc)?;
        tokio::task::spawn(queue.run());
        Ok(Self { sender })
    }

    pub async fn add_user(&self, user: Arc<dyn RevisionUser>, rev_id: i64) -> Result<(), CollaborateError> {
        let (ret, rx) = oneshot::channel();
        let msg = DocCommand::NewConnectedUser { user, rev_id, ret };
        let _ = self.send(msg, rx).await?;
        Ok(())
    }

    pub async fn apply_revision(
        &self,
        user: Arc<dyn RevisionUser>,
        revision: Revision,
    ) -> Result<(), CollaborateError> {
        let (ret, rx) = oneshot::channel();
        let msg = DocCommand::ReceiveRevision { user, revision, ret };
        let _ = self.send(msg, rx).await?;
        Ok(())
    }

    pub async fn document_json(&self) -> CollaborateResult<String> {
        let (ret, rx) = oneshot::channel();
        let msg = DocCommand::GetDocJson { ret };
        self.send(msg, rx).await?
    }

    pub async fn rev_id(&self) -> CollaborateResult<i64> {
        let (ret, rx) = oneshot::channel();
        let msg = DocCommand::GetDocRevId { ret };
        self.send(msg, rx).await?
    }

    async fn send<T>(&self, msg: DocCommand, rx: oneshot::Receiver<T>) -> CollaborateResult<T> {
        let _ = self.sender.send(msg).await.map_err(internal_error)?;
        let result = rx.await.map_err(internal_error)?;
        Ok(result)
    }
}

#[derive(Debug)]
enum DocCommand {
    NewConnectedUser {
        user: Arc<dyn RevisionUser>,
        rev_id: i64,
        ret: oneshot::Sender<CollaborateResult<()>>,
    },
    ReceiveRevision {
        user: Arc<dyn RevisionUser>,
        revision: Revision,
        ret: oneshot::Sender<CollaborateResult<()>>,
    },
    GetDocJson {
        ret: oneshot::Sender<CollaborateResult<String>>,
    },
    GetDocRevId {
        ret: oneshot::Sender<CollaborateResult<i64>>,
    },
}

struct DocCommandQueue {
    receiver: Option<mpsc::Receiver<DocCommand>>,
    edit_doc: Arc<ServerDocEditor>,
}

impl DocCommandQueue {
    fn new(receiver: mpsc::Receiver<DocCommand>, doc: Doc) -> Result<Self, CollaborateError> {
        let edit_doc = Arc::new(ServerDocEditor::new(doc).map_err(internal_error)?);
        Ok(Self {
            receiver: Some(receiver),
            edit_doc,
        })
    }

    async fn run(mut self) {
        let mut receiver = self
            .receiver
            .take()
            .expect("DocActor's receiver should only take one time");

        let stream = stream! {
            loop {
                match receiver.recv().await {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };
        stream.for_each(|msg| self.handle_message(msg)).await;
    }

    async fn handle_message(&self, msg: DocCommand) {
        match msg {
            DocCommand::NewConnectedUser { user, rev_id, ret } => {
                log::debug!("Receive new doc user: {:?}, rev_id: {}", user, rev_id);
                let _ = ret.send(self.edit_doc.new_doc_user(user, rev_id).await.map_err(internal_error));
            },
            DocCommand::ReceiveRevision { user, revision, ret } => {
                // let revision = (&mut revision).try_into().map_err(internal_error).unwrap();
                let _ = ret.send(
                    self.edit_doc
                        .apply_revision(user, revision)
                        .await
                        .map_err(internal_error),
                );
            },
            DocCommand::GetDocJson { ret } => {
                let edit_context = self.edit_doc.clone();
                let json = spawn_blocking(move || edit_context.document_json())
                    .await
                    .map_err(internal_error);
                let _ = ret.send(json);
            },
            DocCommand::GetDocRevId { ret } => {
                let rev_id = self.edit_doc.rev_id.load(SeqCst);
                let _ = ret.send(Ok(rev_id));
            },
        }
    }
}

#[rustfmt::skip]
//                                ┌──────────────────────┐     ┌────────────┐
//                           ┌───▶│ RevisionSynchronizer │────▶│  Document  │
//                           │    └──────────────────────┘     └────────────┘
//     ┌────────────────┐    │
// ───▶│ServerDocEditor │────┤
//     └────────────────┘    │
//                           │
//                           │    ┌────────┐       ┌────────────┐
//                           └───▶│ Users  │◆──────│RevisionUser│
//                                └────────┘       └────────────┘
pub struct ServerDocEditor {
    pub doc_id: String,
    pub rev_id: AtomicI64,
    synchronizer: Arc<RevisionSynchronizer>,
    users: DashMap<String, Arc<dyn RevisionUser>>,
}

impl ServerDocEditor {
    pub fn new(doc: Doc) -> Result<Self, OTError> {
        let delta = RichTextDelta::from_bytes(&doc.data)?;
        let users = DashMap::new();
        let synchronizer = Arc::new(RevisionSynchronizer::new(
            &doc.id,
            doc.rev_id,
            Document::from_delta(delta),
        ));

        Ok(Self {
            doc_id: doc.id.clone(),
            rev_id: AtomicI64::new(doc.rev_id),
            synchronizer,
            users,
        })
    }

    #[tracing::instrument(
        level = "debug",
        skip(self, user),
        fields(
            user_id = %user.user_id(),
            rev_id = %rev_id,
        )
    )]
    pub async fn new_doc_user(&self, user: Arc<dyn RevisionUser>, rev_id: i64) -> Result<(), OTError> {
        self.users.insert(user.user_id(), user.clone());
        self.synchronizer.new_conn(user, rev_id);
        Ok(())
    }

    #[tracing::instrument(
        level = "debug",
        skip(self, user, revision),
        fields(
            cur_rev_id = %self.rev_id.load(SeqCst),
            base_rev_id = %revision.base_rev_id,
            rev_id = %revision.rev_id,
            ),
        err
    )]
    pub async fn apply_revision(&self, user: Arc<dyn RevisionUser>, revision: Revision) -> Result<(), OTError> {
        self.users.insert(user.user_id(), user.clone());
        self.synchronizer.apply_revision(user, revision).unwrap();
        Ok(())
    }

    pub fn document_json(&self) -> String { self.synchronizer.doc_json() }
}