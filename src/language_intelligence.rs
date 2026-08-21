use oxc_allocator::Allocator;
use oxc_ast::ast::{Class, Function, MethodDefinition};
use oxc_ast_visit::{
    Visit,
    walk::{walk_class, walk_function, walk_method_definition, walk_program},
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

pub struct SymbolIndex {
    symbols: Vec<Symbol>,
}

struct Symbol {
    span: Span,
    breadcrumb: String,
}

impl SymbolIndex {
    pub fn new(source_url: &str, source: &str) -> Option<Self> {
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(source_url).unwrap_or_else(|_| SourceType::tsx());
        let parsed = Parser::new(&allocator, source, source_type).parse();
        if parsed.panicked {
            return None;
        }
        let mut visitor = SymbolVisitor {
            stack: Vec::new(),
            symbols: Vec::new(),
        };
        visitor.visit_program(&parsed.program);
        Some(Self {
            symbols: visitor.symbols,
        })
    }

    pub fn breadcrumb(&self, source: &str, line: u32, utf16_column: u32) -> Option<String> {
        let offset = u32::try_from(utf16_position_to_byte(source, line, utf16_column)?).ok()?;
        self.symbols
            .iter()
            .filter(|symbol| symbol.span.start <= offset && offset <= symbol.span.end)
            .max_by_key(|symbol| {
                (
                    symbol.breadcrumb.matches('.').count(),
                    u32::MAX - symbol.span.size(),
                )
            })
            .map(|symbol| symbol.breadcrumb.clone())
    }
}

pub fn breadcrumb(source_url: &str, source: &str, line: u32, utf16_column: u32) -> Option<String> {
    SymbolIndex::new(source_url, source)?.breadcrumb(source, line, utf16_column)
}

struct SymbolVisitor {
    stack: Vec<String>,
    symbols: Vec<Symbol>,
}

impl SymbolVisitor {
    fn enter(&mut self, span: Span, name: Option<String>, visit: impl FnOnce(&mut Self)) {
        if let Some(name) = name {
            self.stack.push(name);
            self.symbols.push(Symbol {
                span,
                breadcrumb: self.stack.join("."),
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

fn utf16_position_to_byte(source: &str, line: u32, column: u32) -> Option<usize> {
    let target_line = line.max(1);
    let mut current_line = 1;
    let mut line_start = 0;
    for (byte, character) in source.char_indices() {
        if current_line == target_line {
            break;
        }
        if character == '\n' {
            current_line += 1;
            line_start = byte + 1;
        }
    }
    if current_line != target_line {
        return None;
    }
    let remainder = &source[line_start..];
    let line_end = remainder.find('\n').unwrap_or(remainder.len());
    let line = remainder[..line_end]
        .strip_suffix('\r')
        .unwrap_or(&remainder[..line_end]);
    let target = column.saturating_sub(1) as usize;
    let mut utf16 = 0;
    for (byte, character) in line.char_indices() {
        if utf16 >= target {
            return Some(line_start + byte);
        }
        utf16 += character.len_utf16();
    }
    Some(line_start + line.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_class_and_method_at_utf16_position() {
        let source =
            "class Cart {\n  checkout(items: number[]) {\n    return items.length;\n  }\n}";
        let index = SymbolIndex::new("cart.ts", source).unwrap();
        assert_eq!(
            index.breadcrumb(source, 3, 12).as_deref(),
            Some("Cart.checkout")
        );
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
}
