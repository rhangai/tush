use tokio::process::Command;

use crate::{
    base::{LogWriter, ProcessChild},
    unit::base::{UnitDefinitonHelper, UnitRunner},
};

pub struct UnitProcess {
    _inner: (),
}

impl UnitProcess {
    pub fn new() -> Self {
        Self { _inner: () }
    }
}

impl UnitDefinitonHelper for UnitProcess {
    fn create_runner(&self, writer: LogWriter) -> impl UnitRunner {
        let mut command = Command::new("printf");
        command.arg("starting server\ntudo\nbem\n");
        let child = ProcessChild::spawn(command, writer).unwrap();
        UnitProcessRunner { child }
    }
}

struct UnitProcessRunner {
    child: ProcessChild,
}
impl UnitRunner for UnitProcessRunner {
    async fn wait(&mut self) -> bool {
        _ = self.child.wait().await;
        true
    }
    async fn kill(mut self) {
        _ = self.child.kill().await;
    }
}
