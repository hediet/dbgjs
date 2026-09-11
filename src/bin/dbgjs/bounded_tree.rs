use std::collections::BTreeMap;
use std::io::IsTerminal;

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy)]
pub(crate) struct TreeRenderOptions {
    pub(crate) maximum_lines: usize,
    pub(crate) maximum_width: Option<usize>,
}

impl TreeRenderOptions {
    pub(crate) fn terminal(maximum_lines: usize, trim_width: bool) -> Self {
        let maximum_width = trim_width
            .then(|| {
                std::io::stdout()
                    .is_terminal()
                    .then(terminal_size::terminal_size)
                    .flatten()
            })
            .flatten()
            .map(|(terminal_size::Width(width), _)| usize::from(width));
        Self {
            maximum_lines,
            maximum_width,
        }
    }
}

pub(crate) trait TreeAggregate: Clone + Default {
    fn merge(&mut self, other: &Self);
}

pub(crate) struct BoundedTree<Aggregate, Leaf> {
    children: BTreeMap<String, BoundedTree<Aggregate, Leaf>>,
    leaf: Option<Leaf>,
    aggregate: Aggregate,
    leaf_count: usize,
}

impl<Aggregate: Default, Leaf> Default for BoundedTree<Aggregate, Leaf> {
    fn default() -> Self {
        Self {
            children: BTreeMap::new(),
            leaf: None,
            aggregate: Aggregate::default(),
            leaf_count: 0,
        }
    }
}

impl<Aggregate, Leaf> BoundedTree<Aggregate, Leaf>
where
    Aggregate: TreeAggregate,
{
    pub(crate) fn insert(
        &mut self,
        path: impl IntoIterator<Item = String>,
        aggregate: Aggregate,
        leaf: Leaf,
    ) {
        let mut node = self;
        node.aggregate.merge(&aggregate);
        node.leaf_count += 1;
        for component in path {
            node = node.children.entry(component).or_default();
            node.aggregate.merge(&aggregate);
            node.leaf_count += 1;
        }
        node.leaf = Some(leaf);
    }

    pub(crate) fn aggregate(&self) -> &Aggregate {
        &self.aggregate
    }

    pub(crate) fn leaf(&self) -> Option<&Leaf> {
        self.leaf.as_ref()
    }

    pub(crate) fn children(&self) -> &BTreeMap<String, Self> {
        &self.children
    }

    pub(crate) fn leaf_count(&self) -> usize {
        self.leaf_count
    }

    pub(crate) fn render<Style>(
        &self,
        style: &Style,
        expand_leaves: bool,
        maximum_lines: usize,
    ) -> Vec<String>
    where
        Style: BoundedTreeStyle<Aggregate, Leaf>,
    {
        self.render_children(style, "", expand_leaves, maximum_lines)
    }

    pub(crate) fn render_with_options<Style>(
        &self,
        style: &Style,
        expand_leaves: bool,
        options: TreeRenderOptions,
    ) -> Vec<String>
    where
        Style: BoundedTreeStyle<Aggregate, Leaf>,
    {
        self.render(style, expand_leaves, options.maximum_lines)
            .into_iter()
            .map(|line| trim_to_width(&line, options.maximum_width))
            .collect()
    }

    fn render_children<Style>(
        &self,
        style: &Style,
        prefix: &str,
        expand_leaves: bool,
        budget: usize,
    ) -> Vec<String>
    where
        Style: BoundedTreeStyle<Aggregate, Leaf>,
    {
        const LONG_LIST: usize = 24;

        if budget == 0 {
            return Vec::new();
        }

        let mut children = self
            .children
            .iter()
            .map(|(name, child)| collapse_tree_label(name, child))
            .collect::<Vec<_>>();
        children.sort_by_key(|(_, child)| std::cmp::Reverse(style.sort_weight(child.aggregate())));
        if budget != usize::MAX && children.len() <= LONG_LIST && budget < children.len() {
            let aggregate = children
                .iter()
                .fold(Aggregate::default(), |mut total, (_, child)| {
                    total.merge(child.aggregate());
                    total
                });
            return vec![format!(
                "{prefix}└─ … {}",
                style.render_all_pruned(children.len(), self.leaf_count, &aggregate)
            )];
        }
        let visible = if budget == usize::MAX {
            children.len()
        } else if children.len() <= LONG_LIST {
            children.len().min(budget)
        } else {
            children.len().min(budget.saturating_sub(1))
        };
        let hidden_items = children.len().saturating_sub(visible);
        let hidden_leaves = children[visible..]
            .iter()
            .map(|(_, child)| child.leaf_count)
            .sum::<usize>();
        let hidden_aggregate =
            children[visible..]
                .iter()
                .fold(Aggregate::default(), |mut total, (_, child)| {
                    total.merge(&child.aggregate);
                    total
                });
        let output_len = visible + usize::from(hidden_items > 0);
        let descendant_budget = budget.saturating_sub(output_len);
        let weight = children
            .iter()
            .take(visible)
            .map(|(_, child)| style.expansion_weight(child, expand_leaves))
            .sum::<u64>();
        let mut output = Vec::new();
        for (index, (label, child)) in children.into_iter().take(visible).enumerate() {
            let last = index + 1 == output_len;
            let branch = if last { "└─" } else { "├─" };
            let child_budget = if budget == usize::MAX {
                usize::MAX
            } else if weight == 0 {
                0
            } else {
                ((descendant_budget as u128 * style.expansion_weight(child, expand_leaves) as u128)
                    / weight as u128) as usize
            };
            let children_pruned =
                child_budget == 0 && style.expansion_weight(child, expand_leaves) > 0;
            let all_children_pruned = child_budget != usize::MAX
                && !child.children.is_empty()
                && child.children.len() <= LONG_LIST
                && child_budget < child.children.len();
            output.push(format!(
                "{prefix}{branch} {}{}",
                style.render_node(&label, child, prefix, expand_leaves),
                if all_children_pruned {
                    format!("  [{} children pruned]", child.children.len())
                } else if children_pruned {
                    "  [children pruned]".to_owned()
                } else {
                    String::new()
                }
            ));
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            if !all_children_pruned {
                output.extend(child.render_children(
                    style,
                    &child_prefix,
                    expand_leaves,
                    child_budget,
                ));
            }
            if child.children.is_empty() && !all_children_pruned {
                output.extend(style.render_leaf_children(
                    &child_prefix,
                    child,
                    child_budget,
                    expand_leaves,
                ));
            }
        }
        if hidden_items > 0 {
            output.push(format!(
                "{prefix}└─ … {}",
                style.render_omitted(hidden_items, hidden_leaves, &hidden_aggregate)
            ));
        }
        if budget != usize::MAX {
            output.truncate(budget);
        }
        output
    }
}

fn trim_to_width(line: &str, maximum_width: Option<usize>) -> String {
    let Some(maximum_width) = maximum_width else {
        return line.to_owned();
    };
    if visible_width(line) <= maximum_width {
        return line.to_owned();
    }
    if maximum_width == 0 {
        return String::new();
    }
    let content_width = maximum_width - 1;
    let mut result = String::new();
    let mut width = 0;
    let mut characters = line.chars().peekable();
    let mut styled = false;
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            result.push(character);
            for character in characters.by_ref() {
                result.push(character);
                if character.is_ascii_alphabetic() {
                    styled = character != 'm' || !result.ends_with("[0m");
                    break;
                }
            }
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    if styled {
        result.push_str("\u{1b}[0m");
    }
    result
}

fn visible_width(line: &str) -> usize {
    let mut width = 0;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            width += UnicodeWidthChar::width(character).unwrap_or(0);
        }
    }
    width
}

pub(crate) trait BoundedTreeStyle<Aggregate, Leaf>
where
    Aggregate: TreeAggregate,
{
    fn sort_weight(&self, aggregate: &Aggregate) -> u64;

    fn expansion_weight(&self, node: &BoundedTree<Aggregate, Leaf>, expand_leaves: bool) -> u64;

    fn render_node(
        &self,
        label: &str,
        node: &BoundedTree<Aggregate, Leaf>,
        prefix: &str,
        expand_leaves: bool,
    ) -> String;

    fn render_leaf_children(
        &self,
        prefix: &str,
        node: &BoundedTree<Aggregate, Leaf>,
        budget: usize,
        expand_leaves: bool,
    ) -> Vec<String>;

    fn render_omitted(
        &self,
        hidden_items: usize,
        hidden_leaves: usize,
        aggregate: &Aggregate,
    ) -> String;

    fn render_all_pruned(
        &self,
        child_count: usize,
        hidden_leaves: usize,
        aggregate: &Aggregate,
    ) -> String;
}

fn collapse_tree_label<'a, Aggregate, Leaf>(
    name: &str,
    mut node: &'a BoundedTree<Aggregate, Leaf>,
) -> (String, &'a BoundedTree<Aggregate, Leaf>) {
    let mut label = name.to_owned();
    while node.leaf.is_none() && node.children.len() == 1 {
        let (child_name, child) = node.children.first_key_value().unwrap();
        label.push('/');
        label.push_str(child_name);
        node = child;
    }
    (label, node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Count(u64);

    impl TreeAggregate for Count {
        fn merge(&mut self, other: &Self) {
            self.0 += other.0;
        }
    }

    struct Style;

    impl BoundedTreeStyle<Count, ()> for Style {
        fn sort_weight(&self, aggregate: &Count) -> u64 {
            aggregate.0
        }

        fn expansion_weight(&self, node: &BoundedTree<Count, ()>, _: bool) -> u64 {
            u64::from(!node.children().is_empty())
        }

        fn render_node(&self, label: &str, _: &BoundedTree<Count, ()>, _: &str, _: bool) -> String {
            label.to_owned()
        }

        fn render_leaf_children(
            &self,
            _: &str,
            _: &BoundedTree<Count, ()>,
            _: usize,
            _: bool,
        ) -> Vec<String> {
            Vec::new()
        }

        fn render_omitted(&self, _: usize, _: usize, _: &Count) -> String {
            "omitted".to_owned()
        }

        fn render_all_pruned(&self, children: usize, _: usize, _: &Count) -> String {
            format!("all {children} children pruned")
        }
    }

    #[test]
    fn marks_children_pruned_when_no_descendant_budget_remains() {
        let mut tree = BoundedTree::default();
        tree.insert(["dir".to_owned(), "a".to_owned()], Count(1), ());
        tree.insert(["dir".to_owned(), "b".to_owned()], Count(1), ());
        assert_eq!(
            tree.render(&Style, false, 1),
            vec!["└─ dir  [2 children pruned]"]
        );
    }

    #[test]
    fn collapsed_zero_weight_paths_do_not_consume_sibling_budget() {
        let mut tree = BoundedTree::default();
        tree.insert(
            [
                "collapsed".to_owned(),
                "middle".to_owned(),
                "leaf".to_owned(),
            ],
            Count(1),
            (),
        );
        for leaf in ["a", "b", "c", "d"] {
            tree.insert(["expand".to_owned(), leaf.to_owned()], Count(1), ());
        }
        let rendered = tree.render(&Style, false, 6).join("\n");
        for leaf in ["a", "b", "c", "d"] {
            assert!(rendered.contains(leaf), "{rendered}");
        }
    }

    #[test]
    fn trims_rendered_lines_to_terminal_width() {
        let mut tree = BoundedTree::default();
        tree.insert(["a very long process label".to_owned()], Count(1), ());
        assert_eq!(
            tree.render_with_options(
                &Style,
                false,
                TreeRenderOptions {
                    maximum_lines: usize::MAX,
                    maximum_width: Some(12),
                }
            ),
            vec!["└─ a very l…"]
        );
    }

    #[test]
    fn trims_ansi_styled_lines_by_visible_width_and_resets_style() {
        let line = "\u{1b}[48;5;238mnon-javascript process\u{1b}[0m";
        let trimmed = trim_to_width(line, Some(8));
        assert_eq!(visible_width(&trimmed), 8);
        assert_eq!(trimmed, "\u{1b}[48;5;238mnon-jav…\u{1b}[0m");
    }
}
