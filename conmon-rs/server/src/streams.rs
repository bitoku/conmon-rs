//! Pseudo terminal implementation.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
};
use anyhow::Result;
use getset::Getters;
use std::os::unix::io::AsRawFd;
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, error, Instrument};

#[derive(Debug, Getters)]
pub struct Streams {
    #[getset(get = "pub")]
    logger: SharedContainerLog,

    #[getset(get = "pub")]
    attach: SharedContainerAttach,

    pub message_rx_stdout: UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stdout: UnboundedSender<Message>,

    pub message_rx_stderr: UnboundedReceiver<Message>,

    #[getset(get = "pub")]
    message_tx_stderr: UnboundedSender<Message>,
}

impl Streams {
    /// Create a new Streams instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new IO streams");

        let (message_tx_stdout, message_rx_stdout) = mpsc::unbounded_channel();
        let (message_tx_stderr, message_rx_stderr) = mpsc::unbounded_channel();

        Ok(Self {
            logger,
            attach,
            message_rx_stdout,
            message_tx_stdout,
            message_rx_stderr,
            message_tx_stderr,
        })
    }

    pub fn handle_stdio_receive(
        &self,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        token: CancellationToken,
    ) {
        debug!("Start reading from IO streams");
        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stdout().clone();

        let token_clone = token.clone();
        if let Some(stdin) = stdin {
            task::spawn(
                async move {
                    if let Err(e) =
                        ContainerIO::read_loop_stdin(stdin.as_raw_fd(), attach, token_clone).await
                    {
                        error!("Stdin read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stdin")),
            );
        }

        let attach = self.attach().clone();
        let token_clone = token.clone();
        if let Some(stdout) = stdout {
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop(
                        stdout,
                        Pipe::StdOut,
                        logger,
                        message_tx,
                        attach,
                        token_clone,
                    )
                    .await
                    {
                        error!("Stdout read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stdout")),
            );
        }

        let logger = self.logger().clone();
        let attach = self.attach().clone();
        let message_tx = self.message_tx_stderr().clone();
        if let Some(stderr) = stderr {
            task::spawn(
                async move {
                    if let Err(e) = ContainerIO::read_loop(
                        stderr,
                        Pipe::StdErr,
                        logger,
                        message_tx,
                        attach,
                        token,
                    )
                    .await
                    {
                        error!("Stderr read loop failure: {:#}", e);
                    }
                }
                .instrument(debug_span!("stderr")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{attach::SharedContainerAttach, container_log::ContainerLog};
    use anyhow::{bail, Context};
    use std::{process::Stdio, str::from_utf8};
    use tokio::process::Command;

    fn msg_string(message: Message) -> Result<String> {
        match message {
            Message::Data(v) => Ok(from_utf8(&v)?.into()),
            _ => bail!("no data in message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_success() -> Result<()> {
        let logger = ContainerLog::new();
        let attach = SharedContainerAttach::default();
        let token = CancellationToken::new();

        let mut sut = Streams::new(logger, attach)?;

        let expected = "hello world";
        let mut child = Command::new("echo")
            .arg("-n")
            .arg(expected)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        sut.handle_stdio_receive(
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
            token.clone(),
        );

        let msg = sut
            .message_rx_stdout
            .recv()
            .await
            .context("no message on stdout")?;

        assert_eq!(msg_string(msg)?, expected);

        // There is no child_reaper instance paying attention to the child we've created,
        // so the read_loops must be cancelled here instead.
        token.cancel();

        let msg = sut
            .message_rx_stdout
            .recv()
            .await
            .context("no message on stdout")?;
        assert_eq!(msg, Message::Done);
        assert!(sut.message_rx_stdout.try_recv().is_err());

        let msg = sut
            .message_rx_stderr
            .recv()
            .await
            .context("no message on stderr")?;
        assert_eq!(msg, Message::Done);
        assert!(sut.message_rx_stderr.try_recv().is_err());

        Ok(())
    }
}
