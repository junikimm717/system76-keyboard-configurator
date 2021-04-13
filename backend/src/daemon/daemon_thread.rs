use futures::{
    channel::{mpsc as async_mpsc, oneshot},
    executor::{block_on, LocalPool},
    prelude::*,
    task::LocalSpawnExt,
};
use futures_timer::Delay;
use glib::clone;
use std::{
    cell::{Cell, RefCell},
    cmp::PartialEq,
    collections::HashMap,
    hash::{Hash, Hasher},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use super::{BoardId, Daemon, Matrix};
use crate::Board;

#[derive(Clone, Debug)]
struct Item<K: Hash + Eq, V> {
    key: K,
    value: V,
}

impl<K: Hash + Eq, V> Item<K, V> {
    fn new(key: K, value: V) -> Self {
        Self { key, value }
    }
}

impl<K: Hash + Eq, V> Hash for Item<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl<K: Hash + Eq, V> PartialEq for Item<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Hash + Eq, V> Eq for Item<K, V> {}

#[derive(Clone, Hash, Eq, PartialEq, Debug)]
enum SetEnum {
    KeyMap(Item<(BoardId, u8, u8, u8), u16>),
    Color(Item<(BoardId, u8), (u8, u8, u8)>),
    Brightness(Item<(BoardId, u8), i32>),
    Mode(Item<(BoardId, u8), (u8, u8)>),
    LedSave(BoardId),
    MatrixGetRate(Item<(), Option<Duration>>),
    Refresh(()),
    Exit(()),
}

#[derive(Debug)]
struct Set {
    inner: SetEnum,
    oneshot: oneshot::Sender<Result<(), String>>,
    cancel: Arc<AtomicBool>,
}

impl Set {
    fn reply(self, resp: Result<(), String>) {
        let _ = self.oneshot.send(resp);
    }
}

#[derive(Clone)]
pub struct ThreadClient {
    cancels: Arc<Mutex<HashMap<SetEnum, Arc<AtomicBool>>>>,
    channel: async_mpsc::UnboundedSender<Set>,
}

impl ThreadClient {
    pub fn new<F: Fn(ThreadResponse) + 'static>(daemon: Box<dyn Daemon>, cb: F) -> Self {
        let (sender, reciever) = async_mpsc::unbounded();
        let client = Self {
            cancels: Arc::new(Mutex::new(HashMap::new())),
            channel: sender,
        };
        let (response_sender, mut response_reciever) = async_mpsc::unbounded();
        glib::MainContext::default().spawn_local(async move {
            while let Some(response) = response_reciever.next().await {
                cb(response)
            }
        });

        Thread::new(daemon, client.clone(), response_sender).spawn(reciever);
        client
    }

    async fn send(&self, set_enum: SetEnum) -> Result<(), String> {
        let mut cancels = self.cancels.lock().unwrap();
        if let Some(cancel) = cancels.remove(&set_enum) {
            cancel.store(true, Ordering::SeqCst);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = oneshot::channel();
        cancels.insert(set_enum.clone(), cancel.clone());
        drop(cancels);

        let _ = self.channel.unbounded_send(Set {
            inner: set_enum,
            oneshot: sender,
            cancel,
        });
        // XXX let caller know it was canceled?
        receiver.await.unwrap_or(Ok(()))
    }

    pub async fn refresh(&self) -> Result<(), String> {
        self.send(SetEnum::Refresh(())).await
    }

    pub async fn keymap_set(
        &self,
        board: BoardId,
        layer: u8,
        output: u8,
        input: u8,
        value: u16,
    ) -> Result<(), String> {
        self.send(SetEnum::KeyMap(Item::new(
            (board, layer, output, input),
            value,
        )))
        .await
    }

    pub async fn set_color(
        &self,
        board: BoardId,
        index: u8,
        color: (u8, u8, u8),
    ) -> Result<(), String> {
        self.send(SetEnum::Color(Item::new((board, index), color)))
            .await
    }

    pub async fn set_brightness(
        &self,
        board: BoardId,
        index: u8,
        brightness: i32,
    ) -> Result<(), String> {
        self.send(SetEnum::Brightness(Item::new((board, index), brightness)))
            .await
    }

    pub async fn set_mode(
        &self,
        board: BoardId,
        layer: u8,
        mode: u8,
        speed: u8,
    ) -> Result<(), String> {
        self.send(SetEnum::Mode(Item::new((board, layer), (mode, speed))))
            .await
    }

    pub async fn set_matrix_get_rate(&self, rate: Option<Duration>) -> Result<(), String> {
        self.send(SetEnum::MatrixGetRate(Item::new((), rate))).await
    }

    pub async fn led_save(&self, board: BoardId) -> Result<(), String> {
        self.send(SetEnum::LedSave(board)).await
    }

    pub fn exit(&self) {
        let _ = block_on(self.send(SetEnum::Exit(())));
    }
}

pub enum ThreadResponse {
    BoardAdded(Board),
    BoardRemoved(BoardId),
}

struct ThreadBoard {
    matrix: Matrix,
    matrix_channel: async_mpsc::UnboundedSender<Matrix>,
}

impl ThreadBoard {
    fn new(matrix_channel: async_mpsc::UnboundedSender<Matrix>) -> Self {
        Self {
            matrix: Matrix::default(),
            matrix_channel,
        }
    }
}

struct Thread {
    daemon: Box<dyn Daemon>,
    boards: RefCell<HashMap<BoardId, ThreadBoard>>,
    client: ThreadClient,
    response_channel: async_mpsc::UnboundedSender<ThreadResponse>,
    matrix_get_rate: Cell<Option<Duration>>,
}

impl Thread {
    fn new(
        daemon: Box<dyn Daemon>,
        client: ThreadClient,
        response_channel: async_mpsc::UnboundedSender<ThreadResponse>,
    ) -> Self {
        Self {
            daemon,
            client,
            response_channel,
            boards: RefCell::new(HashMap::new()),
            matrix_get_rate: Cell::new(None),
        }
    }

    fn spawn(self, mut channel: async_mpsc::UnboundedReceiver<Set>) {
        thread::spawn(move || {
            let mut pool = LocalPool::new();
            let spawner = pool.spawner();

            let self_ = Rc::new(self);

            spawner
                .spawn_local(clone!(@strong self_ => async move {
                    loop {
                        if let Some(rate) = self_.matrix_get_rate.get() {
                            Delay::new(rate).await;
                            self_.matrix_refresh_all();
                        } else {
                            Delay::new(Duration::from_millis(100)).await;
                        }
                    }
                }))
                .unwrap();

            pool.run_until(async move {
                while let Some(set) = channel.next().await {
                    if !self_.handle_set(set) {
                        break;
                    }
                }
            });
        });
    }

    fn handle_set(&self, set: Set) -> bool {
        if set.cancel.load(Ordering::SeqCst) {
            return true;
        }

        let resp = match set.inner {
            SetEnum::KeyMap(Item { key, value }) => {
                self.daemon.keymap_set(key.0, key.1, key.2, key.3, value)
            }
            SetEnum::Color(Item { key, value }) => self.daemon.set_color(key.0, key.1, value),
            SetEnum::Brightness(Item { key, value }) => {
                self.daemon.set_brightness(key.0, key.1, value)
            }
            SetEnum::Mode(Item { key, value }) => {
                self.daemon.set_mode(key.0, key.1, value.0, value.1)
            }
            SetEnum::LedSave(board) => self.daemon.led_save(board),
            SetEnum::MatrixGetRate(Item { value, .. }) => {
                self.matrix_get_rate.set(value);
                Ok(())
            }
            SetEnum::Refresh(()) => self.refresh(),
            SetEnum::Exit(()) => return false,
        };

        set.reply(resp);

        true
    }

    fn matrix_refresh_all(&self) {
        for (k, v) in self.boards.borrow_mut().iter_mut() {
            let matrix = match self.daemon.matrix_get(*k) {
                Ok(matrix) => matrix,
                Err(err) => {
                    error!("Failed to get matrix: {}", err);
                    break;
                }
            };
            if v.matrix != matrix {
                let _ = v.matrix_channel.unbounded_send(matrix.clone());
                v.matrix = matrix;
            }
        }
    }

    fn refresh(&self) -> Result<(), String> {
        let mut boards = self.boards.borrow_mut();

        self.daemon.refresh()?;

        let new_ids = self.daemon.boards()?;

        // Removed boards
        let response_channel = &self.response_channel;
        boards.retain(|id, _| {
            if new_ids.iter().find(|i| *i == id).is_none() {
                // XXX unwrap?
                response_channel
                    .unbounded_send(ThreadResponse::BoardRemoved(*id))
                    .unwrap();
                return false;
            }
            true
        });

        // Added boards
        for i in &new_ids {
            if boards.contains_key(i) {
                continue;
            }

            let (matrix_sender, matrix_reciever) = async_mpsc::unbounded();
            match Board::new(
                self.daemon.as_ref(),
                self.client.clone(),
                *i,
                matrix_reciever,
            ) {
                Ok(board) => {
                    // XXX unwrap?
                    self.response_channel
                        .unbounded_send(ThreadResponse::BoardAdded(board))
                        .unwrap();
                    boards.insert(*i, ThreadBoard::new(matrix_sender));
                }
                Err(err) => error!("Failed to add board: {}", err),
            }
        }

        Ok(())
    }
}