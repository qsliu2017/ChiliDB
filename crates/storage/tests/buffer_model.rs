#[path = "../models/buffer_pool.rs"]
mod buffer_pool;
use buffer_pool::BufferModel;
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
        checker.assert_properties();
        assert_eq!(checker.unique_state_count(), expected_states);
    }
}
