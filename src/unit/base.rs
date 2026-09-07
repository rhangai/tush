use tokio::task::JoinHandle;

trait UnitRunner {
    fn run(self);
}

trait UnitHandle {
    fn abort(&self);
}

trait UnitDescriptor {
    type Runner: UnitRunner;
    type Handle: UnitHandle;

    fn exec(&self) -> (JoinHandle<()>, Self::Handle);
}
