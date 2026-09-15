use oxc_allocator::Allocator;
use oxc_ast::ast::{Class, Function, MethodDefinition};
use oxc_ast_visit::{
    Visit,
    walk::{walk_class, walk_function, walk_method_definition, walk_program},
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use std::sync::Arc;

use crate::source_view::{LineIndex, Position};

pub struct SymbolIndex {
    positions: Arc<LineIndex>,
    root: Option<Box<SymbolNode>>,
}

struct Symbol {
    span: Span,
    breadcrumb: String,
    priority: (usize, u32, usize),
}

struct SymbolNode {
    symbol: Symbol,
    max_end: u32,
    left: Option<Box<SymbolNode>>,
    right: Option<Box<SymbolNode>>,
}

impl SymbolIndex {
    pub fn new(source_url: &str, source: &str) -> Option<Self> {
        Self::with_positions(source_url, source, None)
    }

    pub(crate) fn with_positions(
        source_url: &str,
        source: &str,
        positions: Option<Arc<LineIndex>>,
    ) -> Option<Self> {
        let mut symbols = {
            let allocator = Allocator::default();
            let source_type =
                SourceType::from_path(source_url).unwrap_or_else(|_| SourceType::tsx());
            let parsed = Parser::new(&allocator, source, source_type).parse();
            if parsed.panicked {
                return None;
            }
            let mut visitor = SymbolVisitor {
                stack: Vec::new(),
                symbols: Vec::new(),
            };
            visitor.visit_program(&parsed.program);
            visitor.symbols
        };
        symbols.sort_by_key(|symbol| symbol.span.start);
        let count = symbols.len();
        Some(Self {
            positions: positions.unwrap_or_else(|| Arc::new(LineIndex::new(source))),
            root: SymbolNode::from_sorted(&mut symbols.into_iter(), count),
        })
    }

    pub fn breadcrumb(&self, line: u32, utf16_column: u32) -> Option<String> {
        let offset = u32::try_from(self.positions.clamped_byte_offset(Position {
            line: line.saturating_sub(1),
            column: utf16_column.saturating_sub(1),
        })?)
        .ok()?;
        let mut best = None;
        self.root.as_ref()?.find(offset, &mut best);
        best.map(|symbol| symbol.breadcrumb.clone())
    }
}

impl SymbolNode {
    fn from_sorted(symbols: &mut impl Iterator<Item = Symbol>, count: usize) -> Option<Box<Self>> {
        if count == 0 {
            return None;
        }
        let left = Self::from_sorted(symbols, count / 2);
        let symbol = symbols
            .next()
            .expect("symbol count matches iterator length");
        let right = Self::from_sorted(symbols, count - count / 2 - 1);
        let max_end = symbol
            .span
            .end
            .max(left.as_ref().map_or(0, |node| node.max_end))
            .max(right.as_ref().map_or(0, |node| node.max_end));
        Some(Box::new(Self {
            symbol,
            max_end,
            left,
            right,
        }))
    }

    fn find<'a>(&'a self, offset: u32, best: &mut Option<&'a Symbol>) {
        if offset > self.max_end {
            return;
        }
        if let Some(left) = &self.left {
            left.find(offset, best);
        }
        if self.symbol.span.start > offset {
            return;
        }
        if offset <= self.symbol.span.end
            && best.is_none_or(|previous| self.symbol.priority > previous.priority)
        {
            *best = Some(&self.symbol);
        }
        if let Some(right) = &self.right {
            right.find(offset, best);
        }
    }
}

pub fn breadcrumb(source_url: &str, source: &str, line: u32, utf16_column: u32) -> Option<String> {
    SymbolIndex::new(source_url, source)?.breadcrumb(line, utf16_column)
}

struct SymbolVisitor {
    stack: Vec<String>,
    symbols: Vec<Symbol>,
}

impl SymbolVisitor {
    fn enter(&mut self, span: Span, name: Option<String>, visit: impl FnOnce(&mut Self)) {
        if let Some(name) = name {
            self.stack.push(name);
            let breadcrumb = self.stack.join(".");
            self.symbols.push(Symbol {
                span,
                priority: (
                    breadcrumb.matches('.').count(),
                    u32::MAX - span.size(),
                    self.symbols.len(),
                ),
                breadcrumb,
            });
            visit(self);
            self.stack.pop();
        } else {
            visit(self);
        }
    }
}

impl<'a> Visit<'a> for SymbolVisitor {
    fn visit_program(&mut self, program: &oxc_ast::ast::Program<'a>) {
        walk_program(self, program);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        let name = class.id.as_ref().map(|id| id.name.to_string());
        self.enter(class.span, name, |visitor| walk_class(visitor, class));
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'a>) {
        let name = method.key.static_name().map(|name| name.into_owned());
        self.enter(method.span, name, |visitor| {
            walk_method_definition(visitor, method)
        });
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        let name = function.id.as_ref().map(|id| id.name.to_string());
        self.enter(function.span, name, |visitor| {
            walk_function(visitor, function, flags)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_class_and_method_at_utf16_position() {
        let source =
            "class Cart {\n  checkout(items: number[]) {\n    return items.length;\n  }\n}";
        let index = SymbolIndex::new("cart.ts", source).unwrap();
        assert_eq!(index.breadcrumb(3, 12).as_deref(), Some("Cart.checkout"));
    }

    #[test]
    fn finds_named_function() {
        let source = "function checkout(items: number[]) {\n  return items.length;\n}";
        assert_eq!(
            breadcrumb("cart.ts", source, 2, 5).as_deref(),
            Some("checkout")
        );
    }

    #[test]
    fn preserves_crlf_offsets() {
        let source =
            "class Cart {\r\n  checkout(items: number[]) {\r\n    return items.length;\r\n  }\r\n}";
        assert_eq!(
            breadcrumb("cart.ts", source, 3, 12).as_deref(),
            Some("Cart.checkout")
        );
    }

    #[test]
    fn indexed_spans_match_linear_selection_including_ties_and_boundaries() {
        let mut symbols = [
            (0, 100, "outer"),
            (4, 80, "outer.inner"),
            (10, 20, "outer.inner.first"),
            (20, 30, "outer.inner.second"),
            (20, 30, "outer.inner.last"),
            (25, 25, "outer.inner.last.point"),
            (70, 90, "overlapping"),
            (100, 110, "adjacent"),
        ]
        .into_iter()
        .enumerate()
        .map(|(order, (start, end, name))| Symbol {
            span: Span::new(start, end),
            breadcrumb: name.to_owned(),
            priority: (name.matches('.').count(), u32::MAX - (end - start), order),
        })
        .collect::<Vec<_>>();
        let expected = (0..=111)
            .map(|offset| {
                symbols
                    .iter()
                    .filter(|symbol| symbol.span.start <= offset && offset <= symbol.span.end)
                    .max_by_key(|symbol| symbol.priority)
                    .map(|symbol| symbol.breadcrumb.clone())
            })
            .collect::<Vec<_>>();
        symbols.sort_by_key(|symbol| symbol.span.start);
        let count = symbols.len();
        let root = SymbolNode::from_sorted(&mut symbols.into_iter(), count).unwrap();
        for (offset, expected) in expected.into_iter().enumerate() {
            let mut found = None;
            root.find(offset as u32, &mut found);
            assert_eq!(
                found.map(|symbol| symbol.breadcrumb.clone()),
                expected,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn clamps_columns_and_rejects_missing_lines() {
        let source = "function first() {}\r\nfunction second() {}";
        let index = SymbolIndex::new("fixture.js", source).unwrap();
        assert_eq!(index.breadcrumb(0, 0).as_deref(), Some("first"));
        assert_eq!(index.breadcrumb(1, u32::MAX).as_deref(), Some("first"));
        assert_eq!(index.breadcrumb(2, u32::MAX).as_deref(), Some("second"));
        assert_eq!(index.breadcrumb(3, 1), None);
    }
}
