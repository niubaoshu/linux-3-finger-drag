use std::{
    sync::{
        Arc, atomic::{AtomicBool, Ordering}
    }, 
    time::Duration,
    os::unix::io::AsRawFd
};
use tokio::{
    sync::mpsc::{self, Receiver},
    io::unix::AsyncFd
};
use signal_hook::{self, consts::{SIGINT, SIGTERM}, flag};
use tracing::{debug, error, info, trace};
use tracing_subscriber::fmt::time::ChronoLocal;

use linux_3_finger_drag::{
    init::{config, libinput_init},
    runtime::{
        event_handler::{ControlSignal, GestureTranslator, GtError}, 
        virtual_trackpad
    }
};


#[tokio::main]
async fn main() -> Result<(), GtError> {

    let configs = config::init_cfg();

    match config::init_file_logger(configs.clone()) {
        Some(logger) => logger.init(), 
        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stdout)
                .with_max_level(configs.log_level)
                .with_timer(ChronoLocal::rfc_3339())
                .init();
        }
    };
    println!("[PRE-LOG: INFO]: Logger initialized!"); 

    // handling SIGINT and SIGTERM
    let should_exit = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&should_exit))
        .expect("Failed to register SIGTERM handler");
    flag::register(SIGINT,  Arc::clone(&should_exit))
        .expect("Failed to register SIGINT handler");

    let (sender, recvr) = mpsc::channel::<ControlSignal>(3);
    let vtrackpad = virtual_trackpad::start_handler()?;

    info!("Searching for the trackpad on your device...");

    info!("end evdev search");
    // using a match case here instead of a `?` here so the program can destruct 
    // the virtual trackpad before it exits
    let main_result = match libinput_init::find_real_trackpads() {

        Ok(real_trackpad) => {

            let translator = GestureTranslator::new(
                vtrackpad, 
                configs,
                sender
            );
            run_main_event_loop(
                translator, 
                recvr, 
                &should_exit, 
                real_trackpad
            ).await
        },
        Err(e) => Err(GtError::from(e))
    };

    // the program arrives here if either a signal is received, 
    // or there was some issue during initialization
    info!("Cleaning up and exiting...");
    
    // Cleanup: access vtrackpad through translator if available
    if let Ok(mut translator) = main_result {
        translator.vtp.mouse_up()?;      // just in case
        translator.vtp.destruct()?;      // we don't need virtual devices cluttering the system
        info!("Clean up successful.");
        Ok(())
    } else {
        main_result.map(|_| ())
    }
}


// This function is placed in `main.rs` since it's essentially a 
// part of `main`, and I wanted to break it out so the `main` isn't
// too sprawling
async fn run_main_event_loop(
    mut translator: GestureTranslator,
    recvr: Receiver<ControlSignal>,
    should_exit: &Arc<AtomicBool>,
    real_trackpad: input::Libinput
) -> Result<GestureTranslator, GtError> {

    // spawn 1 separate thread to handle mouse_up_delay timeouts
    debug!("Creating new thread to manage drag end timer");
    let mut vtp_clone = translator.vtp.clone();
    let delay = translator.cfg.drag_end_delay;

    let fork_fn = async move {
        vtp_clone.handle_mouse_up_timeout(delay, recvr)
            .await
            .map_err(GtError::from)
    };

    let mouse_up_listener = tokio::spawn(fork_fn);

    info!("linux-3-finger-drag started successfully!");

    // Wrap the libinput file descriptor for async event-driven polling
    let fd_raw = real_trackpad.as_raw_fd();
    let async_fd = AsyncFd::new(fd_raw)
        .expect("Failed to create AsyncFd for libinput file descriptor");
    
    // We need to move real_trackpad into a position where we can use it with the async_fd
    // Since AsyncFd only wraps the FD, we keep real_trackpad separate
    let mut real_trackpad = real_trackpad;

    loop {
        tokio::select! {
            biased;
            
            // Wait for libinput events (touchpad activity)
            Ok(mut guard) = async_fd.readable() => {
                // Clear the ready state
                guard.clear_ready();
                
                // Process all available events
                if let Err(e) = real_trackpad.dispatch() {
                    error!("A {} error occured in reading device buffer: {}", e.kind(), e);
                }

                for event in &mut real_trackpad {
                    trace!("Event received from libinput");

                    // Process the gesture
                    if let Err(e) = translator.translate_gesture(event).await { 
                        error!("{:?}", e); 
                    }
                }
                
                // Check if mouse_up_listener crashed (once per batch)
                if mouse_up_listener.is_finished() {
                    let fork_err = mouse_up_listener.await?.unwrap_err();
                    error!("Error raised in fork: {:?}", fork_err);
                    return Err(fork_err);
                }
            }
            
            // Periodically check for exit signal
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if should_exit.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }

    debug!("Joining delay timer thread");
    translator.send_signal(ControlSignal::TerminateThread).await?;

    // Wait for the mouse_up_listener to finish
    mouse_up_listener.await??;
    
    // Return translator for cleanup
    Ok(translator)
}