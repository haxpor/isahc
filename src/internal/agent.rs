//! Curl agent that executes multiple requests simultaneously.

use crossbeam_channel::{self, Sender, Receiver};
use curl;
use curl::multi::WaitFd;
use error::Error;
use slab::Slab;
use std::slice;
use std::sync::{Arc, Weak};
use std::sync::atomic::*;
use std::thread;
use std::time::Duration;
use super::notify;
use super::request::*;

const AGENT_THREAD_NAME: &'static str = "curl agent";
const DEFAULT_TIMEOUT_MS: u64 = 1000;

/// Create an agent that executes multiple curl requests simultaneously.
///
/// The agent maintains a background thread that multiplexes all active requests using a single "multi" handle.
pub fn create() -> Result<Handle, Error> {
    let (message_tx, message_rx) = crossbeam_channel::unbounded();
    let (notify_tx, notify_rx) = notify::create()?;

    let handle_inner = Arc::new(HandleInner {
        message_tx,
        notify_tx,
        thread_terminated: AtomicBool::default(),
    });
    let handle_weak = Arc::downgrade(&handle_inner);

    thread::Builder::new().name(String::from(AGENT_THREAD_NAME)).spawn(move || {
        let agent = Agent {
            multi: curl::multi::Multi::new(),
            message_rx,
            notify_rx,
            requests: Slab::new(),
            close_requested: false,
            handle: handle_weak,
        };

        // Intentionally panic the thread if an error occurs.
        agent.run().unwrap();
    })?;

    Ok(Handle {
        inner: handle_inner,
    })
}

/// Handle to an agent. Handles can be sent between threads, shared, and cloned.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<HandleInner>,
}

/// Actual handle to an agent. Only one of these exists per agent.
struct HandleInner {
    /// Used to send messages to the agent.
    message_tx: Sender<Message>,

    /// Used to wake up the agent thread while it is polling.
    notify_tx: notify::NotifySender,

    /// Indicates that the agent thread has exited.
    thread_terminated: AtomicBool,
}

impl Handle {
    /// Begin executing a request with this agent.
    pub fn begin_execute(&self, request: CurlRequest) -> Result<(), Error> {
        request.0.get_ref().set_agent(self.clone());

        self.inner.send_message(Message::BeginRequest(request))
    }

    /// Cancel a request by its token.
    pub fn cancel_request(&self, token: usize) -> Result<(), Error> {
        self.inner.send_message(Message::Cancel(token))
    }

    /// Unpause a request by its token.
    pub fn unpause_write(&self, token: usize) -> Result<(), Error> {
        self.inner.send_message(Message::UnpauseWrite(token))
    }
}

impl HandleInner {
    /// Send a message to the associated agent.
    ///
    /// If the agent is not connected, an error is returned.
    fn send_message(&self, message: Message) -> Result<(), Error> {
        if self.thread_terminated.load(Ordering::SeqCst) {
            error!("agent thread terminated prematurely");
            return Err(Error::Internal);
        }

        self.message_tx.send(message);
        self.notify_tx.notify();

        Ok(())
    }
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        self.send_message(Message::Close).is_ok();
    }
}

/// A message sent from the main thread to the agent thread.
enum Message {
    Cancel(usize),
    Close,
    BeginRequest(CurlRequest),
    UnpauseWrite(usize),
}

/// Internal state of the agent thread.
struct Agent {
    /// A curl multi handle, of course.
    multi: curl::multi::Multi,

    /// Incoming message from the main thread.
    message_rx: Receiver<Message>,

    /// Used to wake up the agent when polling.
    notify_rx: notify::NotifyReceiver,

    /// Contains all of the active requests.
    requests: Slab<curl::multi::Easy2Handle<CurlHandler>>,

    /// Indicates if the thread has been requested to stop.
    close_requested: bool,

    /// Weak reference to a handle, used to communicate back to handles.
    handle: Weak<HandleInner>,
}

impl Agent {
    /// Run the agent in the current thread until requested to stop.
    fn run(mut self) -> Result<(), Error> {
        #[allow(unused_assignments)]
        let mut wait_fd = None;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;

            let mut fd = WaitFd::new();
            fd.set_fd(self.notify_rx.as_raw_fd());
            fd.poll_on_read(true);

            wait_fd = Some(fd);
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;

            let mut fd = WaitFd::new();
            fd.set_fd(self.notify_rx.as_raw_socket() as i32);
            fd.poll_on_read(true);

            wait_fd = Some(fd);
        }

        let wait_fds = match wait_fd.as_mut() {
            Some(mut fd) => slice::from_mut(fd),
            None => {
                warn!("polling interruption is not supported on your platform");
                &mut []
            },
        };

        debug!("agent ready");

        // Agent main loop.
        loop {
            if self.close_requested && self.requests.is_empty() {
                break;
            }

            self.poll_messages()?;

            // Determine the blocking timeout value.
            let timeout = self.multi.get_timeout()?.unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS));

            // Block until activity is detected or the timeout passes.
            trace!("polling with timeout of {:?}", timeout);
            self.multi.wait(wait_fds, timeout)?;

            // We might have woken up early from the notify fd, so drain its queue.
            if self.notify_rx.drain() {
                trace!("woke up from notify fd");
            }

            // Perform any pending reads or writes and handle any state changes.
            self.dispatch()?;
        }

        debug!("agent shutting down");

        self.multi.close()?;

        Ok(())
    }

    /// Polls the message channel for new messages from any agent handles.
    ///
    /// If there are no active requests right now, this function will block until a message is received.
    fn poll_messages(&mut self) -> Result<(), Error> {
        loop {
            if self.requests.is_empty() {
                match self.message_rx.recv() {
                    Some(message) => self.handle_message(message)?,
                    None => {
                        warn!("agent handle disconnected without close message");
                        self.close_requested = true;
                        break;
                    },
                }
            } else {
                match self.message_rx.try_recv() {
                    Some(message) => self.handle_message(message)?,
                    None => break,
                }
            }
        }

        Ok(())
    }

    fn handle_message(&mut self, message: Message) -> Result<(), Error> {
        match message {
            Message::Close => {
                trace!("agent close requested");
                self.close_requested = true;
            },
            Message::BeginRequest(request) => {
                let mut handle = self.multi.add2(request.0)?;
                let mut entry = self.requests.vacant_entry();

                handle.get_ref().set_token(entry.key());
                handle.set_token(entry.key())?;

                entry.insert(handle);
            },
            Message::Cancel(token) => {
                if self.requests.contains(token) {
                    let request = self.requests.remove(token);
                    let request = self.multi.remove2(request)?;
                    drop(request);
                }
            },
            Message::UnpauseWrite(token) => {
                if let Some(request) = self.requests.get(token) {
                    request.unpause_write()?;
                } else {
                    warn!("received unpause request for unknown request token: {}", token);
                }
            },
        }

        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), Error> {
        self.multi.perform()?;

        let mut messages = Vec::new();
        self.multi.messages(|message| {
            if let Some(result) = message.result() {
                if let Ok(token) = message.token() {
                    messages.push((token, result));
                }
            }
        });

        for (token, result) in messages {
            match result {
                Ok(()) => self.complete_request(token)?,
                Err(e) => {
                    debug!("curl error: {}", e);
                    self.fail_request(token, e.into())?;
                },
            };
        }

        Ok(())
    }

    fn complete_request(&mut self, token: usize) -> Result<(), Error> {
        let handle = self.requests.remove(token);
        let handle = self.multi.remove2(handle)?;
        handle.get_ref().complete();

        Ok(())
    }

    fn fail_request(&mut self, token: usize, error: curl::Error) -> Result<(), Error> {
        let handle = self.requests.remove(token);
        let mut handle = self.multi.remove2(handle)?;
        handle.get_mut().fail(error);

        Ok(())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.upgrade() {
            handle.thread_terminated.store(true, Ordering::SeqCst);
        }
    }
}
