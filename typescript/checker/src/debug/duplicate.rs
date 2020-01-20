use backtrace::Backtrace;
use fxhash::FxHashMap;
use std::fmt::Debug;
use swc_common::Spanned;

#[derive(Debug, Default)]
pub(crate) struct DuplicateTracker {
    visited: FxHashMap<String, Backtrace>,
}

impl DuplicateTracker {
    pub fn record<N>(&mut self, node: &N)
    where
        N: Debug + Spanned,
    {
        if node.span().is_dummy() {
            return;
        }

        let key = format!("{:?}", node);
        let bt = Backtrace::new_unresolved();

        self.insert(format!("{:?}", key), bt)
    }

    fn insert(&mut self, key: String, bt: Backtrace) {
        if let Some(bt1) = self.visited.remove(&*key) {
            panic!(
                "Duplicated detected:\n{}\n{:?}\n{:?}",
                key,
                filter(bt1),
                filter(bt)
            )
        }

        self.visited.insert(key, bt);
    }

    pub fn record_all(&mut self, other: DuplicateTracker) {
        for (k, v) in other.visited {
            self.insert(k, v);
        }
    }
}

fn filter(mut bt: Backtrace) -> Backtrace {
    bt.resolve();
    let mut frames: Vec<_> = bt.into();
    let mut done = false;

    frames.retain(|frame| {
        if done {
            return false;
        }

        //
        for symbol in frame.symbols() {
            let name = if let Some(name) = symbol.name().and_then(|s| s.as_str()) {
                name
            } else {
                return false;
            };

            if let Some(filename) = symbol.filename() {
                let s = filename.to_string_lossy();
                if s.contains("fold.rs")
                    || s.contains("validator.rs")
                    || s.contains("vec.rs")
                    || s.contains("/backtrace")
                    || s.contains("libcore")
                {
                    return false;
                }
            }

            if name.contains("Module") {
                done = true;
                // Last one
                return true;
            }
        }

        true
    });

    frames.into()
}
