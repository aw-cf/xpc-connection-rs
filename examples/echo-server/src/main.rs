use futures::stream::StreamExt;
use std::ffi::CString;
use xpc_connection::{Message, MessageError, XpcClient, XpcListener};

async fn handle_client(mut client: XpcClient) {
    println!("New connection");

    loop {
        match client.next().await {
            None => {
                break;
            }
            Some(Message::Error(MessageError::ConnectionInterrupted)) => {
                println!("The connection was interrupted.");
            }
            Some(m) => {
                println!("Server received {:?}", m);
                client.send_message(m);
            }
        }
    }

    println!("The connection was invalidated.");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mach_port_name = CString::new(
        std::env::args()
            .nth(1)
            .expect("Usage: echo-server <mach port name>")
            .as_str(),
    )
    .expect("Failed to convert the mach port name to a CString");

    println!("Waiting for new connections on {mach_port_name:?}",);

    let mut listener = XpcListener::listen(
        &mach_port_name,
        // An example requirement. Whe matching on the common name it's
        // important to anchor to a trusted authority that you know doesn't
        // allow for user-defined common names, otherwise it would be trivial
        // to bypass.
        // Some("anchor apple and certificate leaf[subject.CN] = \"Apple Development: Steven Joruk (Z84S59N9K4)\""),
        None,
        None,
    );

    while let Some(client) = listener.next().await {
        tokio::spawn(handle_client(client));
    }

    println!("Server is shutting down");
}
