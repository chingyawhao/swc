use super::{list::ListFormat, Emitter, Result};
use swc_common::Spanned;
use swc_ecma_ast::*;
use swc_ecma_codegen_macros::emitter;

impl<'a> Emitter<'a> {
    #[emitter]
    pub fn emit_decl(&mut self, node: &Decl) -> Result {
        match *node {
            Decl::Class(ref n) => emit!(n),
            Decl::Fn(ref n) => emit!(n),

            Decl::Var(ref n) => {
                emit!(n);
                semi!(); // VarDecl is also used for for-loops
            }
            Decl::TsEnum(ref n) => emit!(n),
            Decl::TsInterface(ref n) => emit!(n),
            Decl::TsModule(ref n) => emit!(n),
            Decl::TsTypeAlias(ref n) => emit!(n),
        }
    }

    #[emitter]
    pub fn emit_class_decl(&mut self, node: &ClassDecl) -> Result {
        self.emit_leading_comments_of_pos(node.span().lo())?;

        if node.declare {
            keyword!("declare");
            space!();
        }

        for dec in &node.class.decorators {
            emit!(dec);
        }
        keyword!("class");
        space!();
        emit!(node.ident);

        self.emit_class_trailing(&node.class)?;
    }

    #[emitter]
    pub fn emit_fn_decl(&mut self, node: &FnDecl) -> Result {
        self.emit_leading_comments_of_pos(node.span().lo())?;

        if node.declare {
            keyword!("declare");
            space!();
        }

        if node.function.is_async {
            keyword!("async");
            space!();
        }

        keyword!("function");
        if node.function.is_generator {
            punct!("*");
        }

        space!();
        emit!(node.ident);

        self.emit_fn_trailing(&node.function)?;
    }

    #[emitter]
    pub fn emit_var_decl(&mut self, node: &VarDecl) -> Result {
        self.emit_leading_comments_of_pos(node.span.lo())?;

        if node.declare {
            keyword!("declare");
            space!();
        }

        keyword!(node.kind.as_str());
        space!();

        self.emit_list(
            node.span(),
            Some(&node.decls),
            ListFormat::VariableDeclarationList,
        )?;
    }

    #[emitter]
    pub fn emit_var_declarator(&mut self, node: &VarDeclarator) -> Result {
        self.emit_leading_comments_of_pos(node.span().lo())?;

        emit!(node.name);

        if let Some(ref init) = node.init {
            formatting_space!();
            punct!("=");
            formatting_space!();
            emit!(init);
        }
    }
}
