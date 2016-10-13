#![allow(dead_code, unused_variables)]
/**
In many ways, the audio driver controls what the rest of the system
is doing, as the audio hardware works on its own time and controls
when "now" is. Further, we can't switch audio sources until the
hardware asks for packets.

Commands are passed in via a queue. Incoming commands are batched and
only executed upon receipt of a Commit command. The application of
commits is indicated in the command ID field of the timecode in the shared
driver state.

A time of ~0 indicates an unrecoverable error in a stream; a command
ID of ~0 in the timecode indicates a complete device failure.

Drivers are guaranteed to 
*/


use portaudio;
use types;
use std::sync::mpsc;
use std::sync::atomic;
use std::sync::Arc;

#[derive(Debug)]
enum DriverCommand {
    /// Change to a new stream. If the argument is None, plays silence
    /// at the last played sample value.
    ChangeStream(Option<mpsc::Receiver<types::AudioBlock>>),

    /// Resets the time counter to 0
    ZeroTime,

    /// (IMMEDIATE) Commits any outstanding changes. The argument is the new value
    /// for the command ID in the time counter.
    Commit(u16),

    /// (IMMEDIATE) Clear the command queue of any commands that may have been
    /// received.
    Abort,

    /// (IMMEDIATE) Do nothing. This is used for internal purposes, and there is
    /// unlikely to be a reason to use it externally.
    Nop, 
}

struct DriverSharedState {
    // The high 16 bits of this are a command ID; the rest is the time from the last ResetTime command in µs
    time: atomic::AtomicUsize,    
}

struct DriverBackend {
    shared: Arc<DriverSharedState>,
    /// The command queue is guaranteed to be able to hold exactly one
    /// of each type of deferred command. Later instances of a command replace earlier ones.
    deferred_commands: [DriverCommand; 2],

    /// The time as of the last ZeroTime command processed
    time_base: f64,

    /// The ID of the last Commit command
    command_id: u16,
    
    command_queue: mpsc::Receiver<DriverCommand>,

    current_stream: Option<mpsc::Receiver<types::AudioBlock>>,

    last_sample: types::Sample,

    deferred_samples: Vec<types::Sample>,

    deferred_sample_offset: usize,
}

pub struct DriverFrontend {
    shared: Arc<DriverSharedState>,
    command_queue: mpsc::Sender<DriverCommand>,
}

impl DriverBackend {
    fn new(shared: Arc<DriverSharedState>, queue: mpsc::Receiver<DriverCommand>) -> Self {
        let mut backend = DriverBackend{
            shared: shared,
            deferred_commands: Self::default_deferred_commands(),
            time_base: 0.,
            command_id: 0,
            command_queue: queue,
            current_stream: None,
            last_sample: [0;2],
            deferred_samples: Vec::new(),
            deferred_sample_offset: 0,
        };
        backend.receive_command(DriverCommand::ZeroTime, 0.);
        backend
    }

    /// This time value is in seconds since some arbitrary epoch. 
    fn produce_samples(&mut self, time: f64, outbuf: &mut [i32]) {
        // Handle comands
        while let Ok(command) = self.command_queue.try_recv() {
            self.receive_command(command, time);
        }
        
    }

    fn receive_command(&mut self, command: DriverCommand, time: f64) {
        use std::mem::replace;
        match command {
            DriverCommand::ChangeStream(_) => self.deferred_commands[0] = command,
            DriverCommand::ZeroTime => self.deferred_commands[1] = command,
            DriverCommand::Commit(v) => {
                let mut cmdlist = replace(&mut self.deferred_commands, Self::default_deferred_commands());
                for dcmd in cmdlist.iter_mut() {
                    self.process_command(replace(dcmd, DriverCommand::Nop), time);
                }
            },
            DriverCommand::Abort => self.deferred_commands = Self::default_deferred_commands(),
            DriverCommand::Nop => (),
        }
    }

    fn default_deferred_commands() -> [DriverCommand; 2] {
        [DriverCommand::Nop,
         DriverCommand::Nop,
        ]
    }

    fn process_command(&mut self, command: DriverCommand, time: f64) {
        match command {
            DriverCommand::ChangeStream(stream) => self.current_stream = stream,
            DriverCommand::ZeroTime => self.time_base = time,
            _ => (),
        }
    }

    fn signal(&mut self) -> Iter {
        Iter{
            backend: self,
            done: false,
        }
    }
}

struct Iter<'a>{
    backend: &'a mut DriverBackend,
    done: bool,
}

impl <'a> Iterator for Iter<'a> {
    type Item = [i16; 2];

    fn next(&mut self) -> Option<Self::Item> {
        let ref mut be = self.backend;
        while !self.done {
            if be.deferred_sample_offset < be.deferred_samples.len() {
                be.last_sample = be.deferred_samples[be.deferred_sample_offset];
                be.deferred_sample_offset += 1;
                return Some(be.last_sample);
            }

            // try to read the next block.
            let next_block = be.current_stream.as_mut()
                // Disconnected is a NoOp for a non-existent stream
                .map_or(Err(mpsc::TryRecvError::Disconnected),
                        |stream| stream.try_recv());
            match next_block {
                Ok(chunk) => {
                    be.deferred_samples = chunk.block;
                    be.deferred_sample_offset = 0;
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.done = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    be.current_stream = None;
                    self.done = true;
                }
            }
        }

        return None;
    }
}
