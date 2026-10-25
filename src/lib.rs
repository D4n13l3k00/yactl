pub mod config;
pub mod engine;
pub mod logs;
pub mod protocol;
use std::sync::atomic::{AtomicBool, Ordering};
pub static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
pub fn install_signals() {
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
}

pub mod api;
pub mod api_client;
pub mod cli;
pub mod client;
pub mod daemon;
pub mod identity;
pub mod lighting;
pub mod settings;

pub mod clock;
pub mod resource;

pub mod sensors;

pub mod events;
pub mod firmware;
pub mod input;
pub mod volume;
