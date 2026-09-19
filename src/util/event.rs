use tokio::sync::watch;

#[derive(Clone)]
pub struct EventDispatcher {
    sender: watch::Sender<()>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        let sender = watch::Sender::new(());
        Self { sender }
    }

    pub fn create_listener(&self) -> EventListener {
        EventListener {
            receiver: self.sender.subscribe(),
        }
    }

    pub fn trigger(&self) {
        self.sender.send_replace(())
    }
}

pub struct EventListener {
    receiver: tokio::sync::watch::Receiver<()>,
}

impl EventListener {
    pub async fn changed(&mut self) -> bool {
        let result = self.receiver.changed().await;
        result.is_ok()
    }
}
