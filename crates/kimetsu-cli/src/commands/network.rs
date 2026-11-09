//! `kimetsu network`. Membership commands never open a socket.
//! `share` and `request` bind only while they run, and only after join.

use kimetsu_core::KimetsuResult;

use crate::NetworkCommand;

#[cfg(not(feature = "network"))]
use crate::NetworkStatusArgs;

pub(crate) fn network_cmd(command: NetworkCommand) -> KimetsuResult<()> {
    #[cfg(feature = "network")]
    {
        crate::network::run(command).map_err(Into::into)
    }
    #[cfg(not(feature = "network"))]
    {
        disabled(command)
    }
}

#[cfg(not(feature = "network"))]
fn disabled(command: NetworkCommand) -> KimetsuResult<()> {
    match command {
        NetworkCommand::Status(args) => {
            print_offline_status(&args);
            Ok(())
        }
        NetworkCommand::Invite(_)
        | NetworkCommand::Join(_)
        | NetworkCommand::Leave(_)
        | NetworkCommand::Share(_)
        | NetworkCommand::Request(_) => {
            Err("This build of kimetsu does not include the peer network. \
             Release and npm builds do. Nothing is listening."
                .into())
        }
    }
}

#[cfg(not(feature = "network"))]
fn print_offline_status(args: &NetworkStatusArgs) {
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "compiled": false,
                "joined": false,
                "listening": false,
            })
        );
    } else {
        println!("Peer network: not included in this build. Nothing is listening.");
    }
}
