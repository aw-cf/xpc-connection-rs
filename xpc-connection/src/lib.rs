#[allow(
    dead_code,
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    clippy::all
)]
extern crate xpc_connection_sys;

mod message;
pub use message::*;

#[macro_use]
mod dlsym;

use block::ConcreteBlock;
use futures::{
    Stream,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded as unbounded_channel},
};
use std::{
    ffi::{CStr, CString},
    ops::Deref,
    os::raw::{c_char, c_int},
    pin::Pin,
    sync::atomic::AtomicUsize,
    task::Poll,
};

use xpc_connection_sys::{
    XPC_CONNECTION_MACH_SERVICE_LISTENER, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED, dispatch_queue_t,
    xpc_connection_cancel, xpc_connection_create_mach_service, xpc_connection_resume,
    xpc_connection_send_message, xpc_connection_set_event_handler, xpc_connection_t, xpc_object_t,
    xpc_release,
};

dlsym! {
    fn xpc_connection_set_peer_code_sig(*const c_char) -> c_int
}

// A connection's event handler could still be waiting or running when we want
// to drop a connection. We must cancel the handler and wait for the final
// call to a handler to occur, which is always a message containing an
// invalidation error.
fn cancel_and_wait_for_event_handler(connection: xpc_connection_t) {
    let (tx, rx) = std::sync::mpsc::channel();

    let block = ConcreteBlock::new(move |_: xpc_object_t| {
        tx.send(())
            .expect("Failed to announce that the xpc connection's event handler has exited");
    });

    // We must move it from the stack to the heap so that when the libxpc
    // reference count is released we don't double free. This limitation is
    // explained in the blocks crate.
    let block = block.copy();

    unsafe {
        xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);

        xpc_connection_cancel(connection);
    }

    rx.recv()
        .expect("Failed to wait for the xpc connection's event handler to exit");
}

#[derive(Debug)]
pub struct XpcListener {
    connection: xpc_connection_t,
    receiver: UnboundedReceiver<XpcClient>,
    #[allow(dead_code)]
    sender: UnboundedSender<XpcClient>,
}

impl PartialEq for XpcListener {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.connection, other.connection)
    }
}

impl Drop for XpcListener {
    fn drop(&mut self) {
        unsafe {
            cancel_and_wait_for_event_handler(self.connection);
            xpc_release(self.connection as xpc_object_t);
        }
    }
}

impl Stream for XpcListener {
    type Item = XpcClient;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Stream::poll_next(Pin::new(&mut self.receiver), cx)
    }
}

impl XpcListener {
    /// The connection must be a listener.
    fn from_raw(connection: xpc_connection_t, requirement: Option<&'static str>) -> XpcListener {
        let (sender, receiver) = unbounded_channel();
        let sender_clone = sender.clone();

        let mut already_validated = false;

        if let Some(requirement) = requirement {
            if let Some(f) = crate::xpc_connection_set_peer_code_sig.get() {
                let requirement = CString::new(requirement).expect("Invalid requirement string");
                unsafe {
                    f(requirement.as_ptr());
                }

                already_validated = true;
            }
        }

        let block = ConcreteBlock::new(move |event| match xpc_object_to_message(event) {
            Message::Client(mut client) => {
                if already_validated
                    || Self::validate_client_using_audit_token(&client, &requirement)
                {
                    sender_clone.unbounded_send(client).ok()
                } else {
                    unsafe { xpc_connection_cancel(client.connection) };
                    client.event_handler_is_running = false;
                    None
                }
            }
            _ => None,
        });

        // We must move it from the stack to the heap so that when the libxpc
        // reference count is released we don't double free. This limitation is
        // explained in the blocks crate.
        let block = block.copy();

        unsafe {
            xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);
            xpc_connection_resume(connection);
        }

        XpcListener {
            connection,
            receiver,
            sender,
        }
    }

    /// If `requirement` is set then clients will have their code signature
    /// validated before being available. See Apple's documentation on the
    /// language [here](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/RequirementLang/RequirementLang.html).
    ///
    /// On macOS 12 this uses `xpc_connection_set_peer_code_sig`, and if the
    /// `audit_token` feature is enabled then this will use a custom
    /// implementation on older versions of macOS.
    ///
    /// # Panics
    ///
    /// * If `audit_token` feature is used and the `requirement` isn't parsable
    ///   as a `SecRequirement`. This will occur during client validation.
    pub fn listen(
        name: impl AsRef<CStr>,
        requirement: Option<&'static str>,
        queue: Option<dispatch_queue_t>,
    ) -> XpcListener {
        let name = name.as_ref();
        let flags = XPC_CONNECTION_MACH_SERVICE_LISTENER as u64;
        let queue = queue.unwrap_or(std::ptr::null_mut());

        let connection =
            unsafe { xpc_connection_create_mach_service(name.as_ref().as_ptr(), queue, flags) };

        Self::from_raw(connection, requirement)
    }

    #[inline]
    #[cfg(feature = "audit_token")]
    fn validate_client_using_audit_token(client: &XpcClient, requirement: &Option<&str>) -> bool {
        use core_foundation::{base::TCFType, data::CFData};
        use security_framework::os::macos::code_signing::{Flags, GuestAttributes, SecCode};

        let requirement = match requirement {
            Some(r) => r,
            None => return true,
        };

        let requirement = requirement
            .parse()
            .expect("Unable to parse the requirement");

        let token_data = CFData::from_buffer(&client.audit_token());
        let mut attrs = GuestAttributes::new();
        attrs.set_audit_token(token_data.as_concrete_TypeRef());

        if let Ok(code_object) = SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE) {
            return code_object
                .check_validity(Flags::NONE, &requirement)
                .is_ok();
        }

        false
    }

    #[inline]
    #[cfg(not(feature = "audit_token"))]
    fn validate_client_using_audit_token(_client: &XpcClient, _requirement: &Option<&str>) -> bool {
        // TODO: log an error:
        // Attempted to use code signature requirements on an unsupported
        // version of macOS without the `audit_token` feature enabled
        false
    }
}

#[derive(Debug)]
pub struct XpcClient {
    connection: xpc_connection_t,
    event_handler_is_running: bool,
    receiver: UnboundedReceiver<Message>,
    #[allow(dead_code)]
    sender: UnboundedSender<Message>,
}

unsafe impl Send for XpcClient {}

impl PartialEq for XpcClient {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.connection, other.connection)
    }
}

impl Drop for XpcClient {
    fn drop(&mut self) {
        if self.event_handler_is_running {
            cancel_and_wait_for_event_handler(self.connection);
        }

        unsafe { xpc_release(self.connection as xpc_object_t) };
    }
}

impl Stream for XpcClient {
    type Item = Message;

    /// `Poll::Ready(None)` returned in place of `MessageError::ConnectionInvalid`
    /// as it's not recoverable. `MessageError::ConnectionInterrupted` should
    /// be treated as recoverable.
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match Stream::poll_next(Pin::new(&mut self.receiver), cx) {
            Poll::Ready(Some(Message::Error(MessageError::ConnectionInvalid))) => {
                self.event_handler_is_running = false;
                Poll::Ready(None)
            }
            v => v,
        }
    }
}

impl XpcClient {
    /// This sets up a client connection's event handler so that its `Stream`
    /// implementation can be used.
    fn from_raw(connection: xpc_connection_t) -> Self {
        let (sender, receiver) = unbounded_channel();
        let sender_clone = sender.clone();

        // Handle messages received
        let block = ConcreteBlock::new(move |event| {
            let message = xpc_object_to_message(event);
            sender_clone.unbounded_send(message).ok()
        });

        // We must move it from the stack to the heap so that when the libxpc
        // reference count is released we don't double free. This limitation is
        // explained in the blocks crate.
        let block = block.copy();

        unsafe {
            xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);
            xpc_connection_resume(connection);
        }

        XpcClient {
            connection,
            event_handler_is_running: true,
            receiver,
            sender,
        }
    }

    /// The connection isn't established until the first call to `send_message`.
    ///
    /// This is an alias for [`XpcClient::connect_privileged`].
    #[deprecated(
        since = "0.3.0",
        note = "Use connect_privileged or connect_unprivileged"
    )]
    pub fn connect(name: impl AsRef<CStr>) -> Self {
        Self::connect_privileged(name)
    }

    /// The connection isn't established until the first call to `send_message`.
    ///
    /// Connects to a privileged mach port, i.e. a launch daemon.
    pub fn connect_privileged(name: impl AsRef<CStr>) -> Self {
        Self::connect_with_flags(name, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED as u64)
    }

    /// The connection isn't established until the first call to `send_message`.
    ///
    /// Connects to an unprivileged mach port, i.e. a launch agent.
    pub fn connect_unprivileged(name: impl AsRef<CStr>) -> Self {
        Self::connect_with_flags(name, 0)
    }

    /// The connection isn't established until the first call to `send_message`.
    fn connect_with_flags(name: impl AsRef<CStr>, flags: u64) -> Self {
        let name = name.as_ref();
        let connection = unsafe {
            xpc_connection_create_mach_service(name.as_ptr(), std::ptr::null_mut(), flags)
        };
        Self::from_raw(connection)
    }

    /// The connection is established on the first call to `send_message`. You
    /// may receive an error relating to an invalid mach port name or a variety
    /// of other issues.
    pub fn send_message(&self, message: Message) {
        let xpc_object = message_to_xpc_object(message);
        unsafe {
            xpc_connection_send_message(self.connection, xpc_object);
            xpc_release(xpc_object);
        }
    }

    #[cfg(feature = "audit_token")]
    pub fn audit_token(&self) -> [u8; 32] {
        use libc::c_void;

        // This is a private API, but it's also required in order to
        // authenticate XPC clients without requiring a handshake.
        // See https://developer.apple.com/forums/thread/72881 for more info.
        unsafe extern "C" {
            fn xpc_connection_get_audit_token(con: xpc_connection_t, token: *mut c_void);
        }

        let mut token_buffer: [u8; 32] = [0; 32];

        unsafe {
            xpc_connection_get_audit_token(
                self.connection as xpc_connection_t,
                token_buffer.as_mut_ptr() as _,
            )
        }

        token_buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, executor::block_on};
    use std::collections::HashMap;
    use xpc_connection_sys::xpc_connection_cancel;

    // This also tests that the event handler block is only freed once, as a
    // double free is possible if the block isn't copied on to the heap.
    #[test]
    fn event_handler_receives_error_on_close() {
        let mut client = XpcClient::connect_privileged(c"com.apple.blued");

        // Cancelling the connection will cause the event handler to be called
        // with an error message. This will happen under normal circumstances,
        // for example if the service invalidates the connection.
        unsafe { xpc_connection_cancel(client.connection) };

        if let Some(message) = block_on(client.next()) {
            panic!("Expected `None`, but received {:?}", message);
        }
    }

    #[test]
    fn stream_closed_on_drop() {
        let mut client = XpcClient::connect_privileged(c"com.apple.blued");

        let message = Message::Dictionary({
            let mut dictionary = HashMap::new();
            dictionary.insert(c"kCBMsgId".to_owned(), Message::Int64(1));
            dictionary.insert(
                c"kCBMsgArgs".to_owned(),
                Message::Dictionary({
                    let mut temp = HashMap::new();
                    temp.insert(c"kCBMsgArgAlert".to_owned(), Message::Int64(1));
                    temp.insert(
                        c"kCBMsgArgName".to_owned(),
                        Message::String(c"rust".to_owned()),
                    );
                    temp
                }),
            );
            dictionary
        });

        // Can get data while the channel is open
        client.send_message(message);

        let mut count = 0;

        loop {
            match block_on(client.next()) {
                Some(Message::Error(error)) => {
                    panic!("Error: {:?}", error);
                }
                Some(message) => {
                    println!("Received message: {:?}", message);
                    count += 1;

                    // Explained in `event_handler_receives_error_on_close`.
                    unsafe { xpc_connection_cancel(client.connection) };
                }
                None => {
                    // We can't be sure how many buffered messages we'll receive
                    // from blued before the connection is cancelled, but it's
                    // safe to say it should be less than 5.
                    assert!(count < 5);
                    return;
                }
            }
        }
    }
}
