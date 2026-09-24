//! Bounded Stateright model of a proposed asynchronous victim-reservation and
//! page-loading protocol. The production pool still loads synchronously under
//! its metadata mutex; it neither implements nor is shown to refine this model.
//!
//! Protocol:
//! - A victim is reserved before reuse, excluding pins and new acquisitions of
//!   its old page. Dirty data is written back first; writeback failure retains
//!   the old page, mapping, and dirty status.
//! - A Loading mapping is published before the read, so duplicate requests join
//!   the in-flight load. A publisher that loses the mapping recheck releases its
//!   reservation and joins the winner.
//! - Only the loader touches a Loading frame. Successful completion makes the
//!   frame Ready and pins all waiters in one atomic handoff; failure removes the
//!   mapping, frees the frame, and lets waiters retry.
//! - An I/O operation owns its frame until completion; cancelling a waiter never
//!   frees a frame that I/O can still write.
//!
//! Scope: each action is one atomic protocol step, scheduling and I/O errors are
//! arbitrary, and each backend has at most one outstanding request or pin. Only
//! safety and reachability witnesses are checked, over finite bounds with
//! fingerprint deduplication; no progress is claimed without fairness. Bytes,
//! memory ordering, real I/O, panics, crashes, physical I/O cancellation, and
//! late completions are outside the model.
//!
//! An implementation must synchronize mapping publication/recheck, reservation,
//! pinning, and completion handoff; exclude reserved and in-flight frames from
//! victim selection; and track free frames explicitly, since reservations and
//! failed loads break the pool's dense never-unassigned frame prefix.
use stateright::{Model, Property};

#[derive(Clone, Debug)]
pub struct BufferModel {
    pub frame_count: usize,
    pub page_count: usize,
    pub backend_count: usize,
}
impl BufferModel {
    pub fn new(frame_count: usize, page_count: usize, backend_count: usize) -> Self {
        assert!(frame_count > 0 && page_count > 0 && backend_count > 0);
        Self {
            frame_count,
            page_count,
            backend_count,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum Frame {
    Free,
    Reserved {
        target_page_id: usize,
        owner_backend_id: usize,
    },
    Flushing {
        old_page_id: usize,
        target_page_id: usize,
        owner_backend_id: usize,
    },
    Loading {
        page_id: usize,
        io_started: bool,
    },
    Ready {
        page_id: usize,
        pin_count: usize,
        dirty: bool,
    },
}
/// An abstract concurrent database worker/session.
/// Each backend has at most one outstanding request or pin.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum Backend {
    Idle,
    Request {
        page_id: usize,
        retry_after_load_failure: bool,
    },
    Miss {
        page_id: usize,
        retry_after_load_failure: bool,
    },
    Reserved {
        frame_id: usize,
        page_id: usize,
        retry_after_load_failure: bool,
    },
    Waiting {
        frame_id: usize,
        page_id: usize,
        retry_after_load_failure: bool,
    },
    Pinned {
        frame_id: usize,
        page_id: usize,
    },
    Reading {
        frame_id: usize,
        page_id: usize,
    },
    Writing {
        frame_id: usize,
        page_id: usize,
    },
}
struct PinnedPage {
    frame_id: usize,
    page_id: usize,
}

impl Backend {
    fn pin(self) -> Option<PinnedPage> {
        match self {
            Self::Pinned { frame_id, page_id }
            | Self::Reading { frame_id, page_id }
            | Self::Writing { frame_id, page_id } => Some(PinnedPage { frame_id, page_id }),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum Event {
    None,
    RaceReleased { frame_id: usize },
    LoadFailed { frame_id: usize, page_id: usize },
    RetrySucceeded,
    FlushFailed { frame_id: usize, page_id: usize },
}
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct State {
    pub frames: Vec<Frame>,
    pub backends: Vec<Backend>,
    pub page_table: Vec<Option<usize>>,
    // Records the last transition for postcondition checks.
    pub event: Event,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Request { backend_id: usize, page_id: usize },
    Lookup { backend_id: usize },
    Reserve { backend_id: usize, frame_id: usize },
    Publish { backend_id: usize },
    Submit { frame_id: usize },
    LoadDone { frame_id: usize, success: bool },
    FlushDone { frame_id: usize, success: bool },
    Cancel { backend_id: usize },
    Read { backend_id: usize },
    Write { backend_id: usize },
    EndAccess { backend_id: usize },
    Unpin { backend_id: usize },
}

impl State {
    fn join(
        &mut self,
        backend_id: usize,
        frame_id: usize,
        page_id: usize,
        retry_after_load_failure: bool,
    ) {
        match &mut self.frames[frame_id] {
            Frame::Ready { pin_count, .. } => {
                *pin_count += 1;
                self.backends[backend_id] = Backend::Pinned { frame_id, page_id };
            }
            Frame::Loading { .. } => {
                self.backends[backend_id] = Backend::Waiting {
                    frame_id,
                    page_id,
                    retry_after_load_failure,
                }
            }
            _ => {
                self.backends[backend_id] = Backend::Request {
                    page_id,
                    retry_after_load_failure,
                }
            }
        }
    }
    fn published(frame: Frame) -> Option<usize> {
        match frame {
            Frame::Ready {
                page_id: frame_page_id,
                ..
            }
            | Frame::Loading {
                page_id: frame_page_id,
                ..
            } => Some(frame_page_id),
            Frame::Flushing { old_page_id, .. } => Some(old_page_id),
            _ => None,
        }
    }
}

impl Model for BufferModel {
    type State = State;
    type Action = Action;
    fn init_states(&self) -> Vec<State> {
        vec![State {
            frames: vec![Frame::Free; self.frame_count],
            backends: vec![Backend::Idle; self.backend_count],
            page_table: vec![None; self.page_count],
            event: Event::None,
        }]
    }
    fn actions(&self, state: &State, out: &mut Vec<Action>) {
        use Action::*;
        for (backend_id, backend) in state.backends.iter().copied().enumerate() {
            match backend {
                Backend::Idle => {
                    for page_id in 0..self.page_count {
                        out.push(Request {
                            backend_id,
                            page_id,
                        });
                    }
                }
                Backend::Request { page_id, .. } => {
                    if !matches!(
                        state.page_table[page_id].map(|frame_id| state.frames[frame_id]),
                        Some(Frame::Flushing { .. })
                    ) {
                        out.push(Lookup { backend_id });
                    }
                }
                Backend::Miss { page_id, .. } => {
                    // A stale miss may join a newly published page without
                    // waiting for another free victim frame.
                    if matches!(
                        state.page_table[page_id].map(|frame_id| state.frames[frame_id]),
                        Some(Frame::Ready { .. } | Frame::Loading { .. })
                    ) {
                        out.push(Lookup { backend_id });
                    }
                    for (frame_id, frame) in state.frames.iter().enumerate() {
                        if matches!(frame, Frame::Free | Frame::Ready { pin_count: 0, .. }) {
                            out.push(Reserve {
                                backend_id,
                                frame_id,
                            });
                        }
                    }
                }
                Backend::Reserved { frame_id, .. } => {
                    if matches!(state.frames[frame_id], Frame::Reserved { .. }) {
                        out.push(Publish { backend_id });
                    }
                }
                Backend::Waiting { .. } => {
                    out.push(Cancel { backend_id });
                }
                Backend::Pinned { frame_id, .. } => {
                    out.push(Unpin { backend_id });
                    if !state
                        .backends
                        .iter()
                        .any(|backend| matches!(backend, Backend::Writing { frame_id: other_frame_id, .. } if *other_frame_id == frame_id))
                    {
                        out.push(Read { backend_id });
                    }
                    if !state.backends.iter().any(
                        |backend| matches!(backend, Backend::Reading { frame_id: other_frame_id, .. } | Backend::Writing { frame_id: other_frame_id, .. } if *other_frame_id == frame_id),
                    ) {
                        out.push(Write { backend_id });
                    }
                }
                Backend::Reading { .. } | Backend::Writing { .. } => {
                    out.push(EndAccess { backend_id })
                }
            }
        }
        for (frame_id, frame) in state.frames.iter().enumerate() {
            match frame {
                Frame::Loading {
                    io_started: false, ..
                } => out.push(Submit { frame_id }),
                Frame::Loading {
                    io_started: true, ..
                } => {
                    out.push(LoadDone {
                        frame_id,
                        success: true,
                    });
                    out.push(LoadDone {
                        frame_id,
                        success: false,
                    });
                }
                Frame::Flushing { .. } => {
                    out.push(FlushDone {
                        frame_id,
                        success: true,
                    });
                    out.push(FlushDone {
                        frame_id,
                        success: false,
                    });
                }
                _ => {}
            }
        }
    }
    fn next_state(&self, previous_state: &State, action: Action) -> Option<State> {
        use Action::*;
        let mut state = previous_state.clone();
        state.event = Event::None;
        match action {
            Request {
                backend_id,
                page_id,
            } => {
                state.backends[backend_id] = Backend::Request {
                    page_id,
                    retry_after_load_failure: false,
                }
            }
            Lookup { backend_id } => {
                let (Backend::Request {
                    page_id,
                    retry_after_load_failure,
                }
                | Backend::Miss {
                    page_id,
                    retry_after_load_failure,
                }) = state.backends[backend_id]
                else {
                    return None;
                };
                if let Some(frame_id) = state.page_table[page_id] {
                    state.join(backend_id, frame_id, page_id, retry_after_load_failure);
                } else {
                    state.backends[backend_id] = Backend::Miss {
                        page_id,
                        retry_after_load_failure,
                    };
                }
            }
            Reserve {
                backend_id,
                frame_id,
            } => {
                let Backend::Miss {
                    page_id,
                    retry_after_load_failure,
                } = state.backends[backend_id]
                else {
                    return None;
                };
                // Victim selection and reservation form one atomic step.
                // Publication follows in a separate action.
                match state.frames[frame_id] {
                    Frame::Ready {
                        page_id: frame_page_id,
                        pin_count: 0,
                        dirty: true,
                    } => {
                        state.frames[frame_id] = Frame::Flushing {
                            old_page_id: frame_page_id,
                            target_page_id: page_id,
                            owner_backend_id: backend_id,
                        }
                    }
                    Frame::Ready {
                        page_id: frame_page_id,
                        pin_count: 0,
                        dirty: false,
                    } => {
                        state.page_table[frame_page_id] = None;
                        state.frames[frame_id] = Frame::Reserved {
                            target_page_id: page_id,
                            owner_backend_id: backend_id,
                        };
                    }
                    Frame::Free => {
                        state.frames[frame_id] = Frame::Reserved {
                            target_page_id: page_id,
                            owner_backend_id: backend_id,
                        }
                    }
                    _ => return None,
                }
                state.backends[backend_id] = Backend::Reserved {
                    frame_id,
                    page_id,
                    retry_after_load_failure,
                };
            }
            Publish { backend_id } => {
                let Backend::Reserved {
                    frame_id,
                    page_id,
                    retry_after_load_failure,
                } = state.backends[backend_id]
                else {
                    return None;
                };
                // Recheck and publish form the unique-page linearization point.
                if let Some(winner_frame_id) = state.page_table[page_id] {
                    state.frames[frame_id] = Frame::Free;
                    state.join(
                        backend_id,
                        winner_frame_id,
                        page_id,
                        retry_after_load_failure,
                    );
                    state.event = Event::RaceReleased { frame_id };
                } else {
                    state.page_table[page_id] = Some(frame_id);
                    state.frames[frame_id] = Frame::Loading {
                        page_id,
                        io_started: false,
                    };
                    state.backends[backend_id] = Backend::Waiting {
                        frame_id,
                        page_id,
                        retry_after_load_failure,
                    };
                }
            }
            Submit { frame_id } => {
                let Frame::Loading {
                    page_id: frame_page_id,
                    io_started: false,
                } = state.frames[frame_id]
                else {
                    return None;
                };
                state.frames[frame_id] = Frame::Loading {
                    page_id: frame_page_id,
                    io_started: true,
                };
            }
            LoadDone { frame_id, success } => {
                let Frame::Loading {
                    page_id: frame_page_id,
                    io_started: true,
                } = state.frames[frame_id]
                else {
                    return None;
                };
                let mut pin_count = 0;
                for backend in &mut state.backends {
                    if let Backend::Waiting {
                        frame_id: other_frame_id,
                        page_id,
                        retry_after_load_failure,
                    } = *backend
                        && other_frame_id == frame_id
                    {
                        if success {
                            pin_count += 1;
                            if retry_after_load_failure {
                                state.event = Event::RetrySucceeded;
                            }
                            *backend = Backend::Pinned { frame_id, page_id };
                        } else {
                            *backend = Backend::Request {
                                page_id,
                                retry_after_load_failure: true,
                            };
                        }
                    }
                }
                if success {
                    state.frames[frame_id] = Frame::Ready {
                        page_id: frame_page_id,
                        pin_count,
                        dirty: false,
                    };
                } else {
                    state.frames[frame_id] = Frame::Free;
                    state.page_table[frame_page_id] = None;
                    state.event = Event::LoadFailed {
                        frame_id,
                        page_id: frame_page_id,
                    };
                }
            }
            FlushDone { frame_id, success } => {
                let Frame::Flushing {
                    old_page_id,
                    target_page_id,
                    owner_backend_id,
                } = state.frames[frame_id]
                else {
                    return None;
                };
                if success {
                    state.page_table[old_page_id] = None;
                    state.frames[frame_id] = Frame::Reserved {
                        target_page_id,
                        owner_backend_id,
                    };
                } else {
                    state.frames[frame_id] = Frame::Ready {
                        page_id: old_page_id,
                        pin_count: 0,
                        dirty: true,
                    };
                    let Backend::Reserved {
                        frame_id: _,
                        page_id: _,
                        retry_after_load_failure,
                    } = state.backends[owner_backend_id]
                    else {
                        return None;
                    };
                    state.backends[owner_backend_id] = Backend::Request {
                        page_id: target_page_id,
                        retry_after_load_failure,
                    };
                    state.event = Event::FlushFailed {
                        frame_id,
                        page_id: old_page_id,
                    };
                }
            }
            Cancel { backend_id } => state.backends[backend_id] = Backend::Idle, // I/O retains frame ownership until completion.
            Read { backend_id } => {
                let Backend::Pinned { frame_id, page_id } = state.backends[backend_id] else {
                    return None;
                };
                state.backends[backend_id] = Backend::Reading { frame_id, page_id };
            }
            Write { backend_id } => {
                let Backend::Pinned { frame_id, page_id } = state.backends[backend_id] else {
                    return None;
                };
                state.backends[backend_id] = Backend::Writing { frame_id, page_id };
                if let Frame::Ready { dirty, .. } = &mut state.frames[frame_id] {
                    *dirty = true;
                }
            }
            EndAccess { backend_id } => {
                let PinnedPage { frame_id, page_id } = state.backends[backend_id].pin()?;
                state.backends[backend_id] = Backend::Pinned { frame_id, page_id };
            }
            Unpin { backend_id } => {
                let Backend::Pinned { frame_id, .. } = state.backends[backend_id] else {
                    return None;
                };
                if let Frame::Ready { pin_count, .. } = &mut state.frames[frame_id] {
                    *pin_count -= 1;
                }
                state.backends[backend_id] = Backend::Idle;
            }
        }
        Some(state)
    }
    fn properties(&self) -> Vec<Property<Self>> {
        let mut properties = vec![
            Property::<Self>::always("access requires ready matching pinned frame", |_, state| {
                state.backends.iter().all(|backend| {
                    backend.pin().is_none_or(|PinnedPage { frame_id, page_id }| {
                        matches!(state.frames[frame_id], Frame::Ready { page_id: frame_page_id, pin_count, .. }
                            if frame_page_id == page_id && pin_count > 0)
                    })
                })
            }),
            Property::<Self>::always("unique published page independent of map", |_, state| {
                state.frames.iter().enumerate().all(|(frame_id, frame)| {
                    State::published(*frame).is_none_or(|page_id| {
                        !state.frames[..frame_id]
                            .iter()
                            .any(|other| State::published(*other) == Some(page_id))
                    })
                })
            }),
            Property::<Self>::always("mapping bijection", |_, state| {
                state.frames.iter().enumerate().all(|(frame_id, frame)| {
                    State::published(*frame)
                        .is_none_or(|page_id| state.page_table[page_id] == Some(frame_id))
                }) && state
                    .page_table
                    .iter()
                    .enumerate()
                    .all(|(page_id, frame_id)| {
                        frame_id.is_none_or(|frame_id| {
                            State::published(state.frames[frame_id]) == Some(page_id)
                        })
                    })
            }),
            Property::<Self>::always("pin accounting and no pinned eviction", |_, state| {
                state.frames.iter().enumerate().all(|(frame_id, frame)| {
                    let actual_pin_count = state
                        .backends
                        .iter()
                        .filter(|backend| backend.pin().is_some_and(|pin| frame_id == pin.frame_id))
                        .count();
                    match frame {
                        Frame::Ready { pin_count, .. } => *pin_count == actual_pin_count,
                        _ => actual_pin_count == 0,
                    }
                })
            }),
            Property::<Self>::always("reservation and waiter ownership", |_, state| {
                state.backends.iter().enumerate().all(|(owner_backend_id, backend)| match *backend {
                    Backend::Reserved { frame_id, page_id, .. } => matches!(state.frames[frame_id],
                        Frame::Reserved { target_page_id, owner_backend_id: backend_id } | Frame::Flushing { target_page_id, owner_backend_id: backend_id, .. }
                        if backend_id == owner_backend_id && target_page_id == page_id),
                    Backend::Waiting { frame_id, page_id, .. } => matches!(state.frames[frame_id], Frame::Loading { page_id: frame_page_id, .. } if frame_page_id == page_id),
                    _ => true,
                }) && state.frames.iter().enumerate().all(|(frame_id, frame)| match *frame {
                    Frame::Reserved { target_page_id, owner_backend_id } | Frame::Flushing { target_page_id, owner_backend_id, .. } =>
                        matches!(state.backends[owner_backend_id], Backend::Reserved { frame_id: other_frame_id, page_id, .. } if other_frame_id == frame_id && page_id == target_page_id),
                    _ => true,
                })
            }),
            Property::<Self>::always("writer exclusivity", |_, state| {
                state.backends.iter().enumerate().all(|(backend_id, backend)| {
                    if let Backend::Writing { frame_id, .. } = backend {
                        !state.backends.iter().enumerate().any(|(other_backend_id, other_backend)| {
                            backend_id != other_backend_id && matches!(other_backend, Backend::Reading { frame_id: other_frame_id, .. } | Backend::Writing { frame_id: other_frame_id, .. } if frame_id == other_frame_id)
                        })
                    } else { true }
                })
            }),
            Property::<Self>::always(
                "failed load immediately reusable and waiters notified",
                |_, state| {
                    if let Event::LoadFailed { frame_id, page_id } = state.event {
                        state.frames[frame_id] == Frame::Free
                            && state.page_table[page_id].is_none()
                            && !state
                                .backends
                                .iter()
                                .any(|backend| matches!(backend, Backend::Waiting { frame_id: other_frame_id, .. } if *other_frame_id == frame_id))
                    } else {
                        true
                    }
                },
            ),
            Property::<Self>::always("failed writeback retains dirty old page", |_, state| {
                if let Event::FlushFailed { frame_id, page_id } = state.event {
                    state.frames[frame_id]
                        == (Frame::Ready {
                            page_id,
                            pin_count: 0,
                            dirty: true,
                        })
                        && state.page_table[page_id] == Some(frame_id)
                } else {
                    true
                }
            }),
            Property::<Self>::always("publication loser releases extra frame", |_, state| {
                if let Event::RaceReleased { frame_id } = state.event {
                    state.frames[frame_id] == Frame::Free
                        && !state.page_table.contains(&Some(frame_id))
                } else {
                    true
                }
            }),
            Property::<Self>::sometimes("load failure then successful retry", |_, state| {
                state.event == Event::RetrySucceeded
            }),
            Property::<Self>::sometimes(
                "cancelled waiters leave IO owned loading frame",
                |_, state| {
                    state.frames.iter().enumerate().any(|(frame_id, frame)| {
                        matches!(frame, Frame::Loading { io_started: true, .. })
                            && !state
                                .backends
                                .iter()
                                .any(|backend| matches!(backend, Backend::Waiting { frame_id: other_frame_id, .. } if *other_frame_id == frame_id))
                    })
                },
            ),
        ];
        if self.page_count >= 2 || self.backend_count >= 2 {
            properties.push(Property::<Self>::sometimes(
                "dirty writeback failure",
                |_, state| matches!(state.event, Event::FlushFailed { .. }),
            ));
        }
        if self.backend_count >= 2 {
            properties.push(Property::<Self>::sometimes(
                "duplicate waiters join one load",
                |_, state| {
                    state.backends.iter().enumerate().any(|(backend_id, backend)| {
                        if let Backend::Waiting { frame_id, .. } = backend {
                            state.backends[..backend_id]
                                .iter()
                                .any(|other_backend| matches!(other_backend, Backend::Waiting { frame_id: other_frame_id, .. } if frame_id == other_frame_id))
                        } else {
                            false
                        }
                    })
                },
            ));
            if self.frame_count >= 2 {
                properties.push(Property::<Self>::sometimes(
                    "miss publication race loser release",
                    |_, state| matches!(state.event, Event::RaceReleased { .. }),
                ));
            }
        }
        properties
    }
}

#[cfg(test)]
mod tests {
    use super::BufferModel;
    use stateright::{Checker, Model};

    #[test]
    fn proposed_protocol_exhaustive_bfs() {
        for (frame_count, page_count, backend_count, expected_states) in [
            (1, 1, 1, 27),
            (1, 1, 2, 261),
            (1, 2, 2, 1_273),
            (2, 2, 2, 13_061),
            (2, 3, 2, 59_296),
            (2, 2, 3, 209_687),
        ] {
            let checker = BufferModel::new(frame_count, page_count, backend_count)
                .checker()
                .spawn_bfs()
                .join();
            println!(
                "frames={frame_count} pages={page_count} backends={backend_count}: {} unique states, {} visited",
                checker.unique_state_count(),
                checker.state_count()
            );
            let mut discoveries: Vec<_> = checker.discoveries().into_iter().collect();
            discoveries.sort_by_key(|(name, _)| *name);
            for (name, path) in discoveries {
                println!("{name}: {:?}", path.into_actions());
            }
            checker.assert_properties();
            assert_eq!(checker.unique_state_count(), expected_states);
        }
    }
}
