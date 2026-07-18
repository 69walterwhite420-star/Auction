//! E2e helper: builds the participant protocol messages and signs them with
//! wallet keys, so the shell scripts never re-implement the byte protocol.
//!
//! Usage:
//!   participant auction-id <canister-principal> <recipient_hex> <recipient_nonce>
//!   participant lot-id <auction_id_hex> <text_hash_hex>
//!   participant auction-message <chain> <canister-principal> <auction_id_hex> <action> [args]
//!       action: create <recipient_nonce> <duration> <perform_window> <min_entry>
//!             | accept <lot_hex> | return-lot <lot_hex> | return-entry <escrow_hex>
//!             | cancel | ready | vote <done|not_done>
//!   participant sol-sign <keypair.json> <message-file>
//!   participant sol-address <keypair.json>

use auction::auth::{self, Action, Choice};
use candid::Principal;

fn hex_arg(text: &str) -> Vec<u8> {
    hex::decode(text.strip_prefix("0x").unwrap_or(text)).expect("hex argument")
}

fn hex_arg32(text: &str) -> [u8; 32] {
    hex_arg(text).try_into().expect("32-byte hex argument")
}

/// Standard solana keypair file: a JSON array of 64 bytes, secret ‖ public.
fn solana_key(path: &str) -> ed25519_dalek::SigningKey {
    let text = std::fs::read_to_string(path).expect("keypair file");
    let bytes: Vec<u8> = text
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|part| part.trim().parse().expect("keypair byte"))
        .collect();
    let secret: [u8; 32] = bytes[..32].try_into().expect("keypair length");
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = match args.get(1).map(String::as_str) {
        Some("auction-id") => {
            let [canister, recipient, recipient_nonce] = &args[2..] else {
                panic!("auction-id <canister> <recipient_hex> <recipient_nonce>");
            };
            let canister = Principal::from_text(canister).expect("principal");
            hex::encode(
                auth::derive_auction_id(
                    canister.as_slice(),
                    &hex_arg(recipient),
                    recipient_nonce.parse().expect("recipient_nonce"),
                )
                .expect("auction id"),
            )
        }
        Some("lot-id") => {
            let [auction_id, text_hash] = &args[2..] else {
                panic!("lot-id <auction_id_hex> <text_hash_hex>");
            };
            hex::encode(
                auth::derive_lot_id(&hex_arg(auction_id), &hex_arg(text_hash)).expect("lot id"),
            )
        }
        Some("auction-message") => {
            let (chain, canister, auction_id) = (&args[2], &args[3], hex_arg(&args[4]));
            let canister = Principal::from_text(canister).expect("principal");
            let action = match args[5].as_str() {
                "create" => Action::Create {
                    recipient_nonce: args[6].parse().expect("recipient_nonce"),
                    duration: args[7].parse().expect("duration"),
                    perform_window: args[8].parse().expect("perform_window"),
                    min_entry: args[9].parse().expect("min_entry"),
                },
                "accept" => Action::Accept {
                    lot: hex_arg32(&args[6]),
                },
                "return-lot" => Action::ReturnLot {
                    lot: hex_arg32(&args[6]),
                },
                "return-entry" => Action::ReturnEntry {
                    escrow: hex_arg(&args[6]),
                },
                "cancel" => Action::Cancel,
                "ready" => Action::Ready,
                "vote" => Action::Vote(match args[6].as_str() {
                    "done" => Choice::Done,
                    "not_done" => Choice::NotDone,
                    other => panic!("unknown choice {other}"),
                }),
                other => panic!("unknown action {other}"),
            };
            auth::auction_message(chain, &canister.to_text(), &auction_id, &action)
        }
        // The message is text with newlines in it, so it travels by file.
        Some("sol-sign") => {
            let [keypair, message_file] = &args[2..] else {
                panic!("sol-sign <keypair.json> <message-file>");
            };
            use ed25519_dalek::Signer;
            let key = solana_key(keypair);
            let message = std::fs::read(message_file).expect("message file");
            hex::encode(key.sign(&message).to_bytes())
        }
        Some("sol-address") => {
            let [keypair] = &args[2..] else {
                panic!("sol-address <keypair.json>");
            };
            hex::encode(solana_key(keypair).verifying_key().to_bytes())
        }
        _ => panic!("unknown subcommand"),
    };
    // No trailing newline: the caller redirects this into the file that gets
    // signed, and one stray byte is a different message.
    print!("{out}");
}
