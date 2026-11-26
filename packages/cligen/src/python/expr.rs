//! Python syntax, as ruff format (black) writes it: 88 columns, four
//! spaces, a bracket on one line when it fits, else one item per line with
//! a trailing comma (which black then keeps as it is). In the shape of the
//! `codegen` crate: small structs with public fields and a builder method or
//! two, each writing itself through a [`Formatter`] that tracks
//! indentation, its [`Formatter::block`] writing `head:` and the body one
//! level deeper (Python has no braces).

use std::fmt::{self, Write};

use crate::name::{pascal, snake};
use crate::spec::Type;

/// black's line length.
const WIDTH: usize = 88;

/// One level of indentation.
const UNIT: &str = "    ";

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

    /// `text` at the current level, each of its lines indented; an empty one
    /// stays empty.
    ///
    /// # Errors
    ///
    /// Never, writing into a `String`.
    pub fn line(&mut self, text: &str) -> fmt::Result {
        for line in text.split('\n') {
            if !line.is_empty() {
                write!(self.dst, "{}", UNIT.repeat(self.level))?;
            }
            writeln!(self.dst, "{line}")?;
        }
        Ok(())
    }

    /// `head:`, then what `body` writes a level deeper.
    ///
    /// # Errors
    ///
    /// What `body` returns.
    pub fn block(
        &mut self,
        head: &str,
        body: impl FnOnce(&mut Self) -> fmt::Result,
    ) -> fmt::Result {
        self.line(&format!("{head}:"))?;
        self.level += 1;
        body(self)?;
        self.level -= 1;
        Ok(())
    }

    /// `text` as `#` comment lines; nothing for no text.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn comment(&mut self, text: &str) -> fmt::Result {
        for line in text.lines().map(str::trim_end) {
            self.line(&if line.is_empty() {
                "#".to_owned()
            } else {
                format!("# {line}")
            })?;
        }
        Ok(())
    }

    /// `value` at the current level.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn value(&mut self, value: &Value) -> fmt::Result {
        let text = value.render(self.column(), 0);
        self.line(&text)
    }

    /// The column the current level starts at.
    fn column(&self) -> usize {
        self.level * UNIT.len()
    }
}

/// An expression that may break across lines: text, or `head(items)tail`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Text that never breaks.
    Text(String),
    /// `head`, `items` between the brackets, `tail`.
    Brackets {
        /// Before the opening bracket: `return cls`, `id=`.
        head: String,
        /// The brackets: `("(", ")")`.
        brackets: (&'static str, &'static str),
        /// The items.
        items: Vec<Value>,
        /// After the closing bracket: ` -> Pet:`.
        tail: String,
        /// Whether it is a tuple: one item on one line is `(item,)`.
        tuple: bool,
    },
}

impl Value {
    /// `text`.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// `head(items)`.
    #[must_use]
    pub fn call(head: &str, items: Vec<Value>) -> Self {
        Self::Brackets {
            head: head.to_owned(),
            brackets: ("(", ")"),
            items,
            tail: String::new(),
            tuple: false,
        }
    }

    /// `(items)`, a one-tuple `(item,)`.
    #[must_use]
    pub fn tuple(items: Vec<Value>) -> Self {
        Self::Brackets {
            head: String::new(),
            brackets: ("(", ")"),
            items,
            tail: String::new(),
            tuple: true,
        }
    }

    /// `{items}`, each a `"key": value` text.
    #[must_use]
    pub fn dict(items: Vec<Value>) -> Self {
        Self::Brackets {
            head: String::new(),
            brackets: ("{", "}"),
            items,
            tail: String::new(),
            tuple: false,
        }
    }

    /// It, `head` before it: `return `, `name=`.
    #[must_use]
    pub fn prefix(self, before: &str) -> Self {
        match self {
            Self::Text(text) => Self::Text(format!("{before}{text}")),
            Self::Brackets {
                head,
                brackets,
                items,
                tail,
                tuple,
            } => Self::Brackets {
                head: format!("{before}{head}"),
                brackets,
                items,
                tail,
                tuple,
            },
        }
    }

    /// It on one line.
    #[must_use]
    pub fn flat(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Brackets {
                head,
                brackets: (open, close),
                items,
                tail,
                tuple,
            } => {
                let one = if *tuple && items.len() == 1 { "," } else { "" };
                let items: Vec<String> = items.iter().map(Self::flat).collect();
                format!("{head}{open}{}{one}{close}{tail}", items.join(", "))
            }
        }
    }

    /// It starting at `column`, `after` more characters following it: on
    /// one line when that fits, else each item on its own line, a level
    /// deeper, each broken the same way and followed by a comma.
    #[must_use]
    pub fn render(&self, column: usize, after: usize) -> String {
        let flat = self.flat();
        let Self::Brackets {
            head,
            brackets: (open, close),
            items,
            tail,
            ..
        } = self
        else {
            return flat;
        };
        if items.is_empty() || column + flat.chars().count() + after <= WIDTH {
            return flat;
        }
        let mut text = format!("{head}{open}\n");
        for item in items {
            for line in item.render(column + UNIT.len(), 1).split('\n') {
                text.push_str(UNIT);
                text.push_str(line);
                text.push('\n');
            }
            text.insert(text.len() - 1, ',');
        }
        format!("{text}{close}{tail}")
    }
}

/// A docstring, its quotes escaped.
pub struct Docstring<'a>(pub &'a str);

impl Docstring<'_> {
    /// Writes it: one line, or its first line after the quotes and the
    /// closing quotes on their own line.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut text = self
            .0
            .trim()
            .replace('\\', "\\\\")
            .replace("\"\"\"", "\\\"\\\"\\\"");
        if text.ends_with('"') {
            text.insert(text.len() - 1, '\\');
        }
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        if lines.len() == 1 {
            return f.line(&format!("\"\"\"{text}\"\"\""));
        }
        f.line(&format!("\"\"\"{}\n\"\"\"", lines.join("\n")))
    }
}

/// `from module import names`.
pub struct Import {
    /// The module: `.client`.
    pub module: String,
    /// The names it imports.
    pub names: Vec<String>,
}

impl Import {
    /// Writes it, its names in parentheses when they do not fit.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let head = format!("from {} import ", self.module);
        let flat = format!("{head}{}", self.names.join(", "));
        if f.column() + flat.chars().count() <= WIDTH {
            return f.line(&flat);
        }
        f.value(&Value::call(
            &head,
            self.names.iter().cloned().map(Value::Text).collect(),
        ))
    }
}

/// A field of a [`Class`]: `name: ty = default`, its comment above it.
pub struct Field {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: String,
    /// Its default; empty: none.
    pub default: String,
}

/// A statement of a [`Def`].
pub enum Body {
    /// One line.
    Line(String),
    /// An expression that may break.
    Value(Value),
    /// `head:` and its statements.
    Block(String, Vec<Body>),
}

impl Body {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(line) => f.line(line),
            Self::Value(value) => f.value(value),
            Self::Block(head, body) => f.block(head, |f| body.iter().try_for_each(|b| b.fmt(f))),
        }
    }
}

/// `def name(params) -> ret:` and its body; `async def` when asynchronous.
pub struct Def {
    /// Its docstring.
    pub doc: String,
    /// Its decorators, without `@`.
    pub decorators: Vec<String>,
    /// Whether it is `async`.
    pub asynchronous: bool,
    /// Its name.
    pub name: String,
    /// Its parameters, as written.
    pub params: Vec<String>,
    /// Its return type.
    pub ret: String,
    /// Its statements.
    pub body: Vec<Body>,
}

impl Def {
    /// The function `name(params) -> ret`, of no statements yet.
    #[must_use]
    pub fn new(name: &str, params: &[&str], ret: &str, doc: &str) -> Self {
        Self {
            doc: doc.to_owned(),
            decorators: Vec::new(),
            asynchronous: false,
            name: name.to_owned(),
            params: params.iter().map(|param| (*param).to_owned()).collect(),
            ret: ret.to_owned(),
            body: Vec::new(),
        }
    }

    /// Makes it `async`.
    #[must_use]
    pub fn asynchronous(mut self) -> Self {
        self.asynchronous = true;
        self
    }

    /// Adds a decorator.
    #[must_use]
    pub fn decorator(mut self, decorator: &str) -> Self {
        self.decorators.push(decorator.to_owned());
        self
    }

    /// Adds a statement.
    pub fn push(&mut self, body: Body) -> &mut Self {
        self.body.push(body);
        self
    }

    /// Adds a line.
    pub fn line(&mut self, line: impl Into<String>) -> &mut Self {
        self.push(Body::Line(line.into()))
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for decorator in &self.decorators {
            f.line(&format!("@{decorator}"))?;
        }
        let head = Value::Brackets {
            head: format!(
                "{}def {}",
                if self.asynchronous { "async " } else { "" },
                self.name
            ),
            brackets: ("(", ")"),
            items: self.params.iter().cloned().map(Value::Text).collect(),
            tail: format!(" -> {}:", self.ret),
            tuple: false,
        };
        f.value(&head)?;
        f.level += 1;
        if !self.doc.is_empty() {
            Docstring(&self.doc).fmt(f)?;
        }
        if self.body.is_empty() && self.doc.is_empty() {
            f.line("pass")?;
        }
        self.body.iter().try_for_each(|body| body.fmt(f))?;
        f.level -= 1;
        Ok(())
    }
}

/// `class Name(bases):`: its docstring, fields and methods.
pub struct Class {
    /// Its docstring.
    pub doc: String,
    /// Its decorators, without `@`.
    pub decorators: Vec<String>,
    /// Its name.
    pub name: String,
    /// Its bases.
    pub bases: Vec<String>,
    /// Its fields.
    pub fields: Vec<Field>,
    /// Its methods.
    pub defs: Vec<Def>,
}

impl Class {
    /// `@dataclass(frozen=True, kw_only=True) class name:`, of nothing yet.
    #[must_use]
    pub fn dataclass(name: &str, doc: &str) -> Self {
        Self {
            doc: doc.to_owned(),
            decorators: vec!["dataclass(frozen=True, kw_only=True)".to_owned()],
            name: name.to_owned(),
            bases: Vec::new(),
            fields: Vec::new(),
            defs: Vec::new(),
        }
    }

    /// Adds a field; an empty `default` is none.
    pub fn field(&mut self, name: &str, ty: &str, default: &str, doc: &str) -> &mut Self {
        self.fields.push(Field {
            doc: doc.to_owned(),
            name: name.to_owned(),
            ty: ty.to_owned(),
            default: default.to_owned(),
        });
        self
    }

    /// Writes it: docstring, fields, methods, a blank line between each.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for decorator in &self.decorators {
            f.line(&format!("@{decorator}"))?;
        }
        let bases = if self.bases.is_empty() {
            String::new()
        } else {
            format!("({})", self.bases.join(", "))
        };
        f.block(&format!("class {}{bases}", self.name), |f| {
            let mut first = true;
            let mut gap = |f: &mut Formatter<'_>| {
                if !std::mem::take(&mut first) {
                    f.line("")?;
                }
                Ok::<_, fmt::Error>(())
            };
            if !self.doc.is_empty() {
                gap(f)?;
                Docstring(&self.doc).fmt(f)?;
            }
            if !self.fields.is_empty() {
                gap(f)?;
            }
            for field in &self.fields {
                f.comment(&field.doc)?;
                if field.default.is_empty() {
                    f.line(&format!("{}: {}", field.name, field.ty))?;
                } else {
                    f.line(&format!("{}: {} = {}", field.name, field.ty, field.default))?;
                }
            }
            for def in &self.defs {
                gap(f)?;
                def.fmt(f)?;
            }
            if first {
                f.line("pass")?;
            }
            Ok(())
        })
    }
}

/// `class Name(StrEnum):` and a member per value.
pub struct Enum {
    /// Its docstring.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Each member's name and value, quoted.
    pub members: Vec<(String, String)>,
}

impl Enum {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.block(&format!("class {}(StrEnum)", self.name), |f| {
            if !self.doc.is_empty() {
                Docstring(&self.doc).fmt(f)?;
                f.line("")?;
            }
            for (name, value) in &self.members {
                f.line(&format!("{name} = {value}"))?;
            }
            Ok(())
        })
    }
}

/// `Name: TypeAlias = ty`, its comment above it.
pub struct TypeAlias {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// What it names, as written.
    pub ty: String,
}

impl TypeAlias {
    /// Writes it; a type too long for the line goes in parentheses, as
    /// black splits the right-hand side.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.doc)?;
        let line = format!("{}: TypeAlias = {}", self.name, self.ty);
        if f.column() + line.chars().count() <= WIDTH
            || f.column() + UNIT.len() + self.ty.chars().count() > WIDTH
        {
            return f.line(&line);
        }
        f.line(&format!(
            "{}: TypeAlias = (\n{UNIT}{}\n)",
            self.name, self.ty
        ))
    }
}

/// A top-level statement of a [`Module`].
pub enum Item {
    /// A class.
    Class(Class),
    /// A string enum.
    Enum(Enum),
    /// A type alias.
    TypeAlias(TypeAlias),
    /// A function.
    Def(Def),
    /// `value`, its comment above it: `NAME = Operation(…)`.
    Assign(String, Value),
}

impl Item {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Class(item) => item.fmt(f),
            Self::Enum(item) => item.fmt(f),
            Self::TypeAlias(item) => item.fmt(f),
            Self::Def(item) => item.fmt(f),
            Self::Assign(doc, value) => {
                f.comment(doc)?;
                f.value(value)
            }
        }
    }
}

/// A module: its marker, docstring, imports and statements.
pub struct Module {
    /// The generated-code marker.
    pub marker: String,
    /// Its docstring.
    pub doc: String,
    /// Its imports: `__future__`, the standard library, then the package's
    /// own (`.client`), a blank line between each group.
    pub imports: Vec<Import>,
    /// Its statements.
    pub items: Vec<Item>,
}

impl Module {
    /// Writes it: statements two blank lines apart, as isort wants them
    /// after the imports (one before a first statement that is no
    /// definition).
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.marker)?;
        f.line("")?;
        Docstring(&self.doc).fmt(f)?;
        let group = |import: &Import| match import.module.as_str() {
            "__future__" => 0,
            module if module.starts_with('.') => 2,
            _ => 1,
        };
        let mut last = None;
        for import in &self.imports {
            if last != Some(group(import)) {
                f.line("")?;
            }
            last = Some(group(import));
            import.fmt(f)?;
        }
        for (index, item) in self.items.iter().enumerate() {
            let def = matches!(item, Item::Class(_) | Item::Enum(_) | Item::Def(_));
            f.line(if index > 0 || def { "\n" } else { "" })?;
            item.fmt(f)?;
        }
        Ok(())
    }
}

/// Python's keywords, and the builtin names generated annotations use (a
/// field of one would shadow it in its class).
const RESERVED: [&str; 42] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "Any", "bool", "dict", "float", "int", "list", "str",
];

/// A spec type as Python writes it: `list[Pet]`, `str | None`,
/// `dict[str, int]`.
pub struct PyType<'a>(pub &'a Type);

impl fmt::Display for PyType<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Type::String => out.write_str("str"),
            Type::Int | Type::Int32 => out.write_str("int"),
            Type::Float => out.write_str("float"),
            Type::Bool => out.write_str("bool"),
            Type::Any => out.write_str("Any"),
            Type::Optional(inner) if **inner == Type::Any => out.write_str("Any"),
            Type::Named(name) => out.write_str(&PyName(name).class()),
            Type::Array(item) => write!(out, "list[{}]", PyType(item)),
            Type::Map(value) => write!(out, "dict[str, {}]", PyType(value)),
            Type::Optional(inner) => write!(out, "{} | None", PyType(inner)),
        }
    }
}

/// A spec name as Python writes it, reserved words followed by `_`.
pub struct PyName<'a>(pub &'a str);

impl PyName<'_> {
    /// A class: `PetList`.
    #[must_use]
    pub fn class(&self) -> String {
        escape(pascal(self.0), &[])
    }

    /// A field, function or variable: `next_cursor`; `taken` are reserved
    /// too.
    #[must_use]
    pub fn snake(&self, taken: &[&str]) -> String {
        escape(snake(self.0), taken)
    }

    /// A constant: `NO_DATA`.
    #[must_use]
    pub fn constant(&self) -> String {
        snake(self.0).to_uppercase()
    }
}

/// `name`, followed by `_` when reserved or `taken`.
fn escape(name: String, taken: &[&str]) -> String {
    if RESERVED.contains(&name.as_str()) || taken.contains(&name.as_str()) {
        name + "_"
    } else {
        name
    }
}

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

display!(Docstring<'_>, Import, Def, Class, Enum, TypeAlias, Module);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_fit_or_break_with_trailing_commas() {
        let one = Value::tuple(vec![Value::text("\"a.$read\"")]).prefix("templates=");
        assert_eq!(one.flat(), "templates=(\"a.$read\",)");
        let call = Value::call(
            "X = Operation",
            vec![Value::text(format!("id=\"{}\"", "x".repeat(70))), one],
        );
        assert_eq!(
            call.render(0, 0),
            format!(
                "X = Operation(\n    id=\"{}\",\n    templates=(\"a.$read\",),\n)",
                "x".repeat(70)
            )
        );
        let empty = Value::tuple(Vec::new());
        assert_eq!(empty.render(80, 0), "()");
    }

    #[test]
    fn docstrings_imports_and_aliases() {
        assert_eq!(Docstring("A pet.").to_string(), "\"\"\"A pet.\"\"\"\n");
        assert_eq!(
            Docstring("List: `GET /`.\n\nNewest \"first\"").to_string(),
            "\"\"\"List: `GET /`.\n\nNewest \"first\\\"\n\"\"\"\n"
        );
        let names: Vec<String> = (0..12).map(|n| format!("Name{n}")).collect();
        let import = Import {
            module: ".models".into(),
            names,
        };
        assert!(
            import
                .to_string()
                .starts_with("from .models import (\n    Name0,\n")
        );
        let alias = TypeAlias {
            doc: "Either.".into(),
            name: "A".into(),
            ty: "\"B | C\"".into(),
        };
        assert_eq!(alias.to_string(), "# Either.\nA: TypeAlias = \"B | C\"\n");
    }

    #[test]
    fn classes_enums_and_defs() {
        let mut pet = Class::dataclass("Pet", "A pet.");
        pet.field("id", "str", "", "Its id.")
            .field("name", "str | None", "None", "");
        let mut from =
            Def::new("from_json", &["cls", "data: Any"], "Pet", "").decorator("classmethod");
        from.push(Body::Value(Value::call(
            "return cls",
            vec![Value::text("id=data[\"id\"]")],
        )));
        pet.defs.push(from);
        assert_eq!(
            pet.to_string(),
            "@dataclass(frozen=True, kw_only=True)\nclass Pet:\n    \"\"\"A pet.\"\"\"\n\n    # Its id.\n    id: str\n    name: str | None = None\n\n    @classmethod\n    def from_json(cls, data: Any) -> Pet:\n        return cls(id=data[\"id\"])\n"
        );
        let kind = Enum {
            doc: String::new(),
            name: "Kind".into(),
            members: vec![("CAT".into(), "\"cat\"".into())],
        };
        assert_eq!(
            kind.to_string(),
            "class Kind(StrEnum):\n    CAT = \"cat\"\n"
        );
        let long = format!("request: {}Request", "A".repeat(60));
        let mut call = Def::new("get", &["client: Client", &long], "R", "Get.").asynchronous();
        call.push(Body::Block(
            "if x".into(),
            vec![Body::Line("return 1".into())],
        ));
        assert_eq!(
            call.to_string(),
            format!(
                "async def get(\n    client: Client,\n    {long},\n) -> R:\n    \"\"\"Get.\"\"\"\n    if x:\n        return 1\n"
            )
        );
    }
}
