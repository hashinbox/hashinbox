//! TypeScript syntax, as prettier writes it with its defaults (80 columns,
//! two spaces, double quotes), in the shape of the `codegen` crate: small
//! structs with public fields, each writing itself through a [`Formatter`]
//! that tracks indentation; a list stays on one line when it fits, else
//! breaks one item per line with a trailing comma.

use std::fmt::{self, Write};

use crate::name::{camel, pascal};
use crate::out::{Style, fit, is_ident, quote};
use crate::spec::Type;

/// prettier's defaults.
const STYLE: Style = Style {
    width: 80,
    unit: "  ",
};

/// A `String` being written, and how deep the current line is indented.
pub struct Formatter<'a> {
    /// The text.
    dst: &'a mut String,
    /// The indentation level.
    level: usize,
}

impl<'a> Formatter<'a> {
    /// A formatter writing into `dst`.
    pub fn new(dst: &'a mut String) -> Self {
        Self { dst, level: 0 }
    }

    /// `text` at the current level, each of its lines indented.
    ///
    /// # Errors
    ///
    /// Never, writing into a `String`.
    pub fn line(&mut self, text: &str) -> fmt::Result {
        for line in text.split('\n') {
            if !line.is_empty() {
                write!(self.dst, "{}", STYLE.unit.repeat(self.level))?;
            }
            writeln!(self.dst, "{line}")?;
        }
        Ok(())
    }

    /// `head {`, what `body` writes a level deeper, `}`.
    ///
    /// # Errors
    ///
    /// What `body` returns.
    pub fn block(
        &mut self,
        head: &str,
        body: impl FnOnce(&mut Self) -> fmt::Result,
    ) -> fmt::Result {
        self.line(&format!("{head} {{"))?;
        self.level += 1;
        body(self)?;
        self.level -= 1;
        self.line("}")
    }

    /// The column the current level starts at.
    fn column(&self) -> usize {
        self.level * STYLE.unit.len()
    }
}

/// A JSDoc comment: `/** one line */`, or a block of lines.
pub struct JsDoc<'a>(pub &'a str);

impl JsDoc<'_> {
    /// Writes it; nothing for no text.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let text = self.0.replace("*/", "*\\/");
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        match &lines[..] {
            [] => Ok(()),
            [one] => f.line(&format!("/** {} */", one.trim())),
            lines => {
                let lines: Vec<String> = lines
                    .iter()
                    .map(|line| {
                        if line.is_empty() {
                            " *".to_owned()
                        } else {
                            format!(" * {line}")
                        }
                    })
                    .collect();
                f.line(&format!("/**\n{}\n */", lines.join("\n")))
            }
        }
    }
}

/// `import { a, type B } from "module";`.
pub struct Import {
    /// The names, `type ` before a type-only one.
    pub names: Vec<String>,
    /// The module.
    pub from: String,
    /// Whether all are types: `import type`.
    pub types: bool,
}

impl Import {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let head = if self.types {
            "import type "
        } else {
            "import "
        };
        let line = fit(
            STYLE,
            f.column(),
            head,
            ("{", "}"),
            &self.names,
            &format!(" from {};", quote(&self.from)),
            true,
        );
        f.line(&line)
    }
}

/// A property of an [`Interface`].
pub struct Prop {
    /// Its documentation.
    pub doc: String,
    /// Its name on the wire.
    pub name: String,
    /// Whether it may be absent: `name?`.
    pub optional: bool,
    /// Its type.
    pub ty: String,
}

/// `export interface Name { props }`.
pub struct Interface {
    /// Its documentation.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its properties.
    pub props: Vec<Prop>,
}

impl Interface {
    /// An interface `name` of no properties yet.
    #[must_use]
    pub fn new(name: &str, doc: &str) -> Self {
        Self {
            doc: doc.to_owned(),
            name: name.to_owned(),
            props: Vec::new(),
        }
    }

    /// Adds a property.
    pub fn prop(&mut self, name: &str, ty: &str, optional: bool, doc: &str) -> &mut Self {
        self.props.push(Prop {
            doc: doc.to_owned(),
            name: name.to_owned(),
            optional,
            ty: ty.to_owned(),
        });
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        JsDoc(&self.doc).fmt(f)?;
        f.block(&format!("export interface {}", self.name), |f| {
            for prop in &self.props {
                JsDoc(&prop.doc).fmt(f)?;
                f.line(&format!(
                    "{}{}: {};",
                    TsName(&prop.name).key(),
                    if prop.optional { "?" } else { "" },
                    prop.ty
                ))?;
            }
            Ok(())
        })
    }
}

/// `export type Name = a | b;`, broken as prettier breaks a union.
pub struct TypeAlias {
    /// Its documentation.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its members.
    pub members: Vec<String>,
}

impl TypeAlias {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        JsDoc(&self.doc).fmt(f)?;
        let flat = format!("export type {} = {};", self.name, self.members.join(" | "));
        let next = format!("  {};", self.members.join(" | "));
        if f.column() + flat.chars().count() <= STYLE.width {
            f.line(&flat)
        } else if f.column() + next.chars().count() <= STYLE.width {
            f.line(&format!("export type {} =\n{next}", self.name))
        } else {
            let members: Vec<String> = self
                .members
                .iter()
                .map(|member| format!("  | {member}"))
                .collect();
            f.line(&format!(
                "export type {} =\n{};",
                self.name,
                members.join("\n")
            ))
        }
    }
}

/// `export function name(params): ret { return callee(args); }`.
pub struct Function {
    /// Its documentation.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its parameters, as written.
    pub params: Vec<String>,
    /// Its return type.
    pub ret: String,
    /// The function its body returns a call of.
    pub callee: String,
    /// That call's arguments.
    pub args: Vec<String>,
}

impl Function {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        JsDoc(&self.doc).fmt(f)?;
        let head = fit(
            STYLE,
            f.column(),
            &format!("export function {}", self.name),
            ("(", ")"),
            &self.params,
            &format!(": {} {{", self.ret),
            false,
        );
        f.line(&head)?;
        f.level += 1;
        let call = fit(
            STYLE,
            f.column(),
            &format!("return {}", self.callee),
            ("(", ")"),
            &self.args,
            ";",
            false,
        );
        f.line(&call)?;
        f.level -= 1;
        f.line("}")
    }
}

/// `export const name: Type = { key: value, … };`, one entry per line
/// (prettier keeps an object literal broken as written); a value that is an
/// array breaks one item per line when it does not fit.
pub struct Const {
    /// Its documentation.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: String,
    /// Each entry's key and value.
    pub entries: Vec<(String, String)>,
}

impl Const {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        JsDoc(&self.doc).fmt(f)?;
        f.line(&format!("export const {}: {} = {{", self.name, self.ty))?;
        f.level += 1;
        for (key, value) in &self.entries {
            let line = match value
                .strip_prefix('[')
                .and_then(|items| items.strip_suffix(']'))
            {
                Some(items) if !items.is_empty() => {
                    let items: Vec<String> = items.split(", ").map(str::to_owned).collect();
                    fit(
                        STYLE,
                        f.column(),
                        &format!("{key}: "),
                        ("[", "]"),
                        &items,
                        ",",
                        false,
                    )
                }
                _ => format!("{key}: {value},"),
            };
            f.line(&line)?;
        }
        f.level -= 1;
        f.line("};")
    }
}

/// A spec type as TypeScript writes it: `string`, `Pet[]`,
/// `Record<string, number>`, `Owner | null`.
pub struct TsType<'a>(pub &'a Type);

impl fmt::Display for TsType<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Type::String => out.write_str("string"),
            Type::Int | Type::Int32 | Type::Float => out.write_str("number"),
            Type::Bool => out.write_str("boolean"),
            Type::Any => out.write_str("unknown"),
            Type::Named(name) => out.write_str(name),
            Type::Array(item) if matches!(**item, Type::Optional(_)) => {
                write!(out, "({})[]", TsType(item))
            }
            Type::Array(item) => write!(out, "{}[]", TsType(item)),
            Type::Map(value) => write!(out, "Record<string, {}>", TsType(value)),
            Type::Optional(inner) => write!(out, "{} | null", TsType(inner)),
        }
    }
}

/// A spec name as TypeScript writes it.
pub struct TsName<'a>(pub &'a str);

impl TsName<'_> {
    /// A function's name: camel case, `_` after a reserved word
    /// (`read_wallet` → `readWallet`).
    #[must_use]
    pub fn function(&self) -> String {
        let name = camel(self.0);
        if KEYWORDS.contains(&name.as_str()) {
            format!("{name}_")
        } else {
            name
        }
    }

    /// A type's name: `read_wallet` → `ReadWallet`.
    #[must_use]
    pub fn pascal(&self) -> String {
        pascal(self.0)
    }

    /// A property name: as is when an identifier, else quoted.
    #[must_use]
    pub fn key(&self) -> String {
        if is_ident(self.0) {
            self.0.to_owned()
        } else {
            quote(self.0)
        }
    }
}

/// TypeScript's reserved and strict-mode words.
const KEYWORDS: [&str; 45] = [
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "yield",
];

/// `Display` through `fmt` with a fresh formatter.
macro_rules! display {
    ($($ty:ty),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut text = String::new();
                Self::fmt(self, &mut Formatter::new(&mut text))?;
                out.write_str(&text)
            }
        }
    )*};
}

display!(JsDoc<'_>, Import, Interface, TypeAlias, Function, Const);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_like_prettier() {
        let mut pet = Interface::new("Pet", "A pet.");
        pet.prop("id", "string", false, "Its id.")
            .prop("x-tag", "string | null", true, "");
        assert_eq!(
            pet.to_string(),
            "/** A pet. */\nexport interface Pet {\n  /** Its id. */\n  id: string;\n  \"x-tag\"?: string | null;\n}\n"
        );
        let kind = TypeAlias {
            doc: String::new(),
            name: "Kind".into(),
            members: vec!["\"cat\"".into(), "\"dog\"".into()],
        };
        assert_eq!(kind.to_string(), "export type Kind = \"cat\" | \"dog\";\n");
        let params = vec![
            "client: Client".into(),
            "params: ReadPetParams".into(),
            "signal?: AbortSignal".into(),
        ];
        let args = vec![
            "client".into(),
            "readPet.request(params)".into(),
            "signal".into(),
        ];
        let function = Function {
            doc: String::new(),
            name: "readPet".into(),
            params,
            ret: "Promise<Pet>".into(),
            callee: "send<Pet>".into(),
            args,
        };
        let text = "export function readPet(\n  client: Client,\n  params: ReadPetParams,\n  signal?: AbortSignal,\n): Promise<Pet> {\n  return send<Pet>(client, readPet.request(params), signal);\n}\n";
        assert_eq!(function.to_string(), text);
    }

    #[test]
    fn consts_like_prettier() {
        let entries = vec![
            ("id".into(), "\"pets_list\"".into()),
            ("templates".into(), "[\"a.$read\"]".into()),
        ];
        let text = Const {
            doc: String::new(),
            name: "petsListOperation".into(),
            ty: "Operation".into(),
            entries,
        }
        .to_string();
        assert_eq!(
            text,
            "export const petsListOperation: Operation = {\n  id: \"pets_list\",\n  templates: [\"a.$read\"],\n};\n"
        );
    }
}
