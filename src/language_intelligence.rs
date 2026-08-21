use oxc_allocator::Allocator;
use oxc_ast::ast::{Class, Function, MethodDefinition};
use oxc_ast_visit::{
    Visit,
    walk::{walk_class, walk_function, walk_method_definition, walk_program},
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

pub fn breadcrumb(source_url: &str, source: &str, line: u32, utf16_column: u32) -> Option<String> {
    let offset = utf16_position_to_byte(source, line, utf16_column)?;
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(source_url).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return None;
    }
    let mut visitor = BreadcrumbVisitor {
        offset: u32::try_from(offset).ok()?,
        stack: Vec::new(),
        best: Vec::new(),
    };
    visitor.visit_program(&parsed.program);
    (!visitor.best.is_empty()).then(|| visitor.best.join("."))
}

struct BreadcrumbVisitor {
    offset: u32,
    stack: Vec<String>,
    best: Vec<String>,
}

impl BreadcrumbVisitor {
    fn contains(&self, span: Span) -> bool {
        span.start <= self.offset && self.offset <= span.end
    }

    fn enter(&mut self, name: Option<String>, visit: impl FnOnce(&mut Self)) {
        if let Some(name) = name {
            self.stack.push(name);
            if self.stack.len() > self.best.len() {
                self.best.clone_from(&self.stack);
            }
            visit(self);
            self.stack.pop();
        } else {
            visit(self);
        }
    }
}

impl<'a> Visit<'a> for BreadcrumbVisitor {
    fn visit_program(&mut self, program: &oxc_ast::ast::Program<'a>) {
        walk_program(self, program);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        if !self.contains(class.span) {
            return;
        }
        let name = class.id.as_ref().map(|id| id.name.to_string());
        self.enter(name, |visitor| walk_class(visitor, class));
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'a>) {
        if !self.contains(method.span) {
            return;
        }
        let name = method.key.static_name().map(|name| name.into_owned());
        self.enter(name, |visitor| walk_method_definition(visitor, method));
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        if !self.contains(function.span) {
            return;
        }
        let name = function.id.as_ref().map(|id| id.name.to_string());
        self.enter(name, |visitor| walk_function(visitor, function, flags));
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
        assert_eq!(
            breadcrumb("cart.ts", source, 3, 12).as_deref(),
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
