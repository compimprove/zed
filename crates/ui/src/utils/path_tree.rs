use std::collections::BTreeMap;

use gpui::SharedString;

/// A row of a file tree built by [`build_path_tree`].
#[derive(Debug, Clone, PartialEq)]
pub enum PathTreeEntry<P, T> {
    Directory {
        path: P,
        /// The directory name, or several names joined with `/` when
        /// directories that only contain one directory are compacted.
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    Item {
        item: T,
        depth: usize,
    },
}

struct PathTreeNode<P, T> {
    name: SharedString,
    path: Option<P>,
    children: BTreeMap<SharedString, PathTreeNode<P, T>>,
    items: Vec<T>,
}

impl<P, T> PathTreeNode<P, T> {
    fn new(name: SharedString, path: Option<P>) -> Self {
        Self {
            name,
            path,
            children: BTreeMap::new(),
            items: Vec::new(),
        }
    }
}

/// Groups `items` into directories and flattens them into rows: directories
/// sorted by name come before the items of the same directory, which keep
/// their order. Directories that only contain one directory are compacted
/// into one row, and the rows inside collapsed directories are left out.
///
/// `components` returns an item's path components, ending with its own name.
/// `directory_path` returns the path of the directory made of the given
/// leading components, or `None` to skip that directory level.
pub fn build_path_tree<P: Clone, T>(
    items: impl IntoIterator<Item = T>,
    components: impl Fn(&T) -> Vec<SharedString>,
    directory_path: impl Fn(&[SharedString]) -> Option<P>,
    is_expanded: impl Fn(&P) -> bool,
) -> Vec<PathTreeEntry<P, T>> {
    let mut root = PathTreeNode::new(SharedString::default(), None);
    for item in items {
        let components = components(&item);
        let mut current = &mut root;
        for (ix, component) in components
            .iter()
            .enumerate()
            .take(components.len().saturating_sub(1))
        {
            let Some(path) = directory_path(&components[..=ix]) else {
                continue;
            };
            current = current
                .children
                .entry(component.clone())
                .or_insert_with(|| PathTreeNode::new(component.clone(), Some(path)));
        }
        current.items.push(item);
    }

    let mut entries = Vec::new();
    flatten_path_tree(root, 0, &is_expanded, &mut entries);
    entries
}

fn flatten_path_tree<P: Clone, T>(
    node: PathTreeNode<P, T>,
    depth: usize,
    is_expanded: &impl Fn(&P) -> bool,
    entries: &mut Vec<PathTreeEntry<P, T>>,
) {
    for child in node.children.into_values() {
        let (terminal, name) = compact_directory_chain(child);
        let Some(path) = terminal.path.clone() else {
            continue;
        };
        let expanded = is_expanded(&path);
        entries.push(PathTreeEntry::Directory {
            path,
            name,
            depth,
            expanded,
        });
        if expanded {
            flatten_path_tree(terminal, depth + 1, is_expanded, entries);
        }
    }

    entries.extend(
        node.items
            .into_iter()
            .map(|item| PathTreeEntry::Item { item, depth }),
    );
}

fn compact_directory_chain<P, T>(
    mut node: PathTreeNode<P, T>,
) -> (PathTreeNode<P, T>, SharedString) {
    let mut parts = vec![node.name.clone()];
    while node.items.is_empty() && node.children.len() == 1 {
        let Some((_, child)) = node.children.pop_first() else {
            break;
        };
        parts.push(child.name.clone());
        node = child;
    }
    (node, SharedString::from(parts.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(paths: &[&str], collapsed: &[&str]) -> Vec<String> {
        build_path_tree(
            paths.iter().map(|path| path.to_string()),
            |path| {
                path.split('/')
                    .map(|c| SharedString::from(c.to_string()))
                    .collect()
            },
            |components| Some(components.join("/")),
            |path| !collapsed.contains(&path.as_str()),
        )
        .into_iter()
        .map(|entry| match entry {
            PathTreeEntry::Directory {
                name,
                depth,
                expanded,
                ..
            } => format!(
                "{}{} {name}/",
                "  ".repeat(depth),
                if expanded { "v" } else { ">" }
            ),
            PathTreeEntry::Item { item, depth } => format!("{}{item}", "  ".repeat(depth)),
        })
        .collect()
    }

    #[test]
    fn groups_items_by_directory() {
        assert_eq!(
            tree(&["src/b.rs", "src/a.rs", "README.md", "docs/guide.md"], &[]),
            [
                "v docs/",
                "  docs/guide.md",
                "v src/",
                "  src/b.rs",
                "  src/a.rs",
                "README.md",
            ]
        );
    }

    #[test]
    fn compacts_single_directory_chains() {
        assert_eq!(
            tree(
                &["api/.github/workflows/a.yml", "api/.github/workflows/b.yml"],
                &[]
            ),
            [
                "v api/.github/workflows/",
                "  api/.github/workflows/a.yml",
                "  api/.github/workflows/b.yml",
            ]
        );
    }

    #[test]
    fn hides_the_rows_of_collapsed_directories() {
        assert_eq!(
            tree(&["a/x.rs", "a/b/y.rs", "c.rs"], &["a/b"]),
            ["v a/", "  > b/", "  a/x.rs", "c.rs"]
        );
        assert_eq!(tree(&["a/x.rs", "a/b/y.rs"], &["a"]), ["> a/"]);
    }

    #[test]
    fn handles_empty_input() {
        assert!(tree(&[], &[]).is_empty());
    }
}
