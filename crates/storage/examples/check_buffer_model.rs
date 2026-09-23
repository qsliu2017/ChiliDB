#[path = "../models/buffer_pool.rs"]
mod buffer_pool;
use buffer_pool::BufferModel;
use stateright::{Checker, Model};

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|s| {
            s.parse()
                .expect("arguments: frames pages backends (positive integers)")
        })
        .collect();
    let bounds = match args.as_slice() {
        [] => vec![(1, 2, 2), (2, 2, 2), (2, 3, 2), (2, 2, 3)],
        [frame_count, page_count, backend_count] => {
            vec![(*frame_count, *page_count, *backend_count)]
        }
        _ => panic!("usage: check_buffer_model [frames pages backends]"),
    };
    for (frame_count, page_count, backend_count) in bounds {
        let checker = BufferModel::new(frame_count, page_count, backend_count)
            .checker()
            .spawn_bfs()
            .join();
        println!(
            "frames={frame_count} pages={page_count} backends={backend_count}: {} unique states; {} visited; max depth {}",
            checker.unique_state_count(),
            checker.state_count(),
            checker.max_depth()
        );
        let mut discoveries: Vec<_> = checker.discoveries().into_iter().collect();
        discoveries.sort_by_key(|(name, _)| *name);
        for (name, path) in discoveries {
            println!("{name}: {:?}", path.into_actions());
        }
        checker.assert_properties();
    }
}
