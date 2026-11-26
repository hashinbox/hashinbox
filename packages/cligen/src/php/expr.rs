//! PHP syntax, as PER-CS 2.0 writes it: 120 columns, four spaces, a class's
//! or a function's brace on its own line, a control structure's on the same
//! one, an empty body `{}`, a list that does not fit one item per line with
//! a trailing comma. In the shape of the `codegen` crate: small structs with
//! public fields and a builder method or two, each writing itself through a
//! [`Formatter`] that tracks indentation.

use std::collections::BTreeMap;
use std::fmt::{self, Write};

use crate::name::{camel, pascal, snake};
use crate::spec::{Model, Type};

/// PER-CS's soft line length.
const WIDTH: usize = 120;

/// One level of indentation.
const UNIT: &str = "    ";

/// Every model, by name: what a type needs to be written (a union is its
/// variants).
pub type Models<'a> = BTreeMap<&'a str, &'a Model>;

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

    /// `head {`, what `body` writes a level deeper, `}`: a control
    /// structure.
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
        self.indented(body)?;
        self.line("}")
    }

    /// `head`, `{` on its own line, what `body` writes a level deeper, `}`:
    /// a class or an enum.
    ///
    /// # Errors
    ///
    /// What `body` returns.
    pub fn braced(
        &mut self,
        head: &str,
        body: impl FnOnce(&mut Self) -> fmt::Result,
    ) -> fmt::Result {
        self.line(&format!("{head}\n{{"))?;
        self.indented(body)?;
        self.line("}")
    }

    /// What `body` writes, a level deeper.
    fn indented(&mut self, body: impl FnOnce(&mut Self) -> fmt::Result) -> fmt::Result {
        self.level += 1;
        body(self)?;
        self.level -= 1;
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
        /// Before the opening bracket: `return new self`, `Json::list`.
        head: String,
        /// The brackets: `("(", ")")`, `("[", "]")`.
        brackets: (&'static str, &'static str),
        /// The items.
        items: Vec<Value>,
        /// After the closing bracket: `;`, ` : null`.
        tail: String,
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
        }
    }

    /// `[items]`.
    #[must_use]
    pub fn array(items: Vec<Value>) -> Self {
        Self::Brackets {
            head: String::new(),
            brackets: ("[", "]"),
            items,
            tail: String::new(),
        }
    }

    /// It, `before` in front of it: `return `, `'id' => `.
    #[must_use]
    pub fn prefix(self, before: &str) -> Self {
        match self {
            Self::Text(text) => Self::Text(format!("{before}{text}")),
            Self::Brackets {
                head,
                brackets,
                items,
                tail,
            } => Self::Brackets {
                head: format!("{before}{head}"),
                brackets,
                items,
                tail,
            },
        }
    }

    /// It, `after` behind it: `;`, ` : null`.
    #[must_use]
    pub fn suffix(self, after: &str) -> Self {
        match self {
            Self::Text(text) => Self::Text(format!("{text}{after}")),
            Self::Brackets {
                head,
                brackets,
                items,
                tail,
            } => Self::Brackets {
                head,
                brackets,
                items,
                tail: format!("{tail}{after}"),
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
            } => {
                let items: Vec<String> = items.iter().map(Self::flat).collect();
                format!("{head}{open}{}{close}{tail}", items.join(", "))
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

/// `/** … */`: a description, then its tags after a blank line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocBlock {
    /// Its description, each line as written.
    pub text: String,
    /// Its tags: `@param list<Pet> $items`.
    pub tags: Vec<String>,
}

impl DocBlock {
    /// A doc block of `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.trim().to_owned(),
            tags: Vec::new(),
        }
    }

    /// Adds a tag.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Writes it; nothing when it has no text and no tags.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut lines: Vec<String> = self.text.lines().map(str::to_owned).collect();
        if !lines.is_empty() && !self.tags.is_empty() {
            lines.push(String::new());
        }
        for tag in &self.tags {
            let mut tag = tag.lines();
            lines.extend(tag.next().map(str::to_owned));
            lines.extend(tag.map(|line| format!("{UNIT}{line}")));
        }
        if lines.is_empty() {
            return Ok(());
        }
        f.line("/**")?;
        for line in lines {
            let line = line.trim_end().replace("*/", "*\\/");
            f.line(&if line.is_empty() {
                " *".to_owned()
            } else {
                format!(" * {line}")
            })?;
        }
        f.line(" */")
    }
}

/// A statement of a [`Method`].
pub enum Body {
    /// One line.
    Line(String),
    /// An expression that may break.
    Value(Value),
    /// `head {` and its statements: a `foreach`, an `if`.
    Block(String, Vec<Body>),
    /// `head {`, its statements, `};`: a closure assigned.
    Closure(String, Vec<Body>),
    /// `if (condition) { … } elseif (condition) { … }`.
    If(Vec<(String, Vec<Body>)>),
}

impl Body {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(line) => f.line(line),
            Self::Value(value) => f.value(value),
            Self::Block(head, body) => f.block(head, |f| all(f, body)),
            Self::Closure(head, body) => {
                f.line(&format!("{head} {{"))?;
                f.indented(|f| all(f, body))?;
                f.line("};")
            }
            Self::If(arms) => {
                for (index, (condition, body)) in arms.iter().enumerate() {
                    let head = if index == 0 {
                        format!("if ({condition}) {{")
                    } else {
                        format!("}} elseif ({condition}) {{")
                    };
                    f.line(&head)?;
                    f.indented(|f| all(f, body))?;
                }
                f.line("}")
            }
        }
    }
}

/// Each of `body`, written.
fn all(f: &mut Formatter<'_>, body: &[Body]) -> fmt::Result {
    body.iter().try_for_each(|body| body.fmt(f))
}

/// A method, or a function when it has no modifiers.
pub struct Method {
    /// Its doc block.
    pub doc: DocBlock,
    /// `public`, `public static`; empty: a function.
    pub modifiers: String,
    /// Its name.
    pub name: String,
    /// Its parameters, as written.
    pub params: Vec<String>,
    /// Its return type; empty: none (a constructor).
    pub ret: String,
    /// Its statements.
    pub body: Vec<Body>,
}

impl Method {
    /// The function `name(params): ret`, of no statements yet.
    #[must_use]
    pub fn function(name: &str, params: &[&str], ret: &str, doc: DocBlock) -> Self {
        Self {
            doc,
            modifiers: String::new(),
            name: name.to_owned(),
            params: params.iter().map(|param| (*param).to_owned()).collect(),
            ret: ret.to_owned(),
            body: Vec::new(),
        }
    }

    /// The public method `name(params): ret`, of no statements yet.
    #[must_use]
    pub fn public(name: &str, params: &[&str], ret: &str, doc: DocBlock) -> Self {
        Self {
            modifiers: "public".to_owned(),
            ..Self::function(name, params, ret, doc)
        }
    }

    /// Makes it `static`.
    #[must_use]
    pub fn static_(mut self) -> Self {
        self.modifiers.push_str(" static");
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

    /// Writes it: its parameters on one line when they fit (the brace then
    /// on the next), else one per line (the brace after `)`); an empty
    /// body is `{}`.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.doc.fmt(f)?;
        let modifiers = if self.modifiers.is_empty() {
            String::new()
        } else {
            format!("{} ", self.modifiers)
        };
        let head = format!("{modifiers}function {}", self.name);
        let ret = if self.ret.is_empty() {
            String::new()
        } else {
            format!(": {}", self.ret)
        };
        let flat = format!("{head}({}){ret}", self.params.join(", "));
        let empty = self.body.is_empty();
        if f.column() + flat.chars().count() + if empty { 3 } else { 0 } <= WIDTH {
            if empty {
                return f.line(&format!("{flat} {{}}"));
            }
            f.line(&format!("{flat}\n{{"))?;
        } else {
            f.line(&format!("{head}("))?;
            f.indented(|f| {
                self.params
                    .iter()
                    .try_for_each(|param| f.line(&format!("{param},")))
            })?;
            if empty {
                return f.line(&format!("){ret} {{}}"));
            }
            f.line(&format!("){ret} {{"))?;
        }
        f.indented(|f| all(f, &self.body))?;
        f.line("}")
    }
}

/// A promoted constructor parameter: `public Type $name = default`.
pub struct Property {
    /// Its description.
    pub doc: String,
    /// Its name, without `$`.
    pub name: String,
    /// Its native type.
    pub ty: String,
    /// Its PHPDoc type; empty: the native one says it all.
    pub doc_ty: String,
    /// Its default; empty: none.
    pub default: String,
}

/// `final readonly class Name implements …`: its promoted constructor and
/// its methods.
pub struct Class {
    /// Its doc block.
    pub doc: DocBlock,
    /// `final readonly`.
    pub modifiers: String,
    /// Its name.
    pub name: String,
    /// The interfaces it implements.
    pub implements: Vec<String>,
    /// Its properties, each promoted in its constructor; none: no
    /// constructor.
    pub properties: Vec<Property>,
    /// Its methods.
    pub methods: Vec<Method>,
}

impl Class {
    /// The `final readonly` class `name`, of nothing yet.
    #[must_use]
    pub fn readonly(name: &str, doc: DocBlock) -> Self {
        Self {
            doc,
            modifiers: "final readonly".to_owned(),
            name: name.to_owned(),
            implements: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
        }
    }

    /// Adds a property; an empty `default` is none, an empty `doc_ty` the
    /// native type.
    pub fn property(&mut self, name: &str, ty: &str, doc_ty: &str, default: &str, doc: &str) {
        self.properties.push(Property {
            doc: doc.to_owned(),
            name: name.to_owned(),
            ty: ty.to_owned(),
            doc_ty: doc_ty.to_owned(),
            default: default.to_owned(),
        });
    }

    /// Its constructor: each property promoted (those without a default
    /// first), a `@param` for each that has a description or a PHPDoc type.
    fn constructor(&self) -> Option<Method> {
        if self.properties.is_empty() {
            return None;
        }
        let mut doc = DocBlock::default();
        let mut params = Vec::new();
        let (required, optional): (Vec<&Property>, Vec<&Property>) = self
            .properties
            .iter()
            .partition(|property| property.default.is_empty());
        for property in required.into_iter().chain(optional) {
            if !property.doc.is_empty() || !property.doc_ty.is_empty() {
                let ty = if property.doc_ty.is_empty() {
                    &property.ty
                } else {
                    &property.doc_ty
                };
                let tag = format!("@param {ty} ${} {}", property.name, property.doc);
                doc.tags.push(tag.trim_end().to_owned());
            }
            let default = if property.default.is_empty() {
                String::new()
            } else {
                format!(" = {}", property.default)
            };
            params.push(format!(
                "public {} ${}{default}",
                property.ty, property.name
            ));
        }
        Some(Method {
            params,
            ..Method::public("__construct", &[], "", doc)
        })
    }

    /// Writes it: constructor and methods a blank line apart.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.doc.fmt(f)?;
        let implements = if self.implements.is_empty() {
            String::new()
        } else {
            format!(" implements {}", self.implements.join(", "))
        };
        let head = format!("{} class {}{implements}", self.modifiers, self.name);
        let constructor = self.constructor();
        f.braced(&head, |f| {
            for (index, method) in constructor.iter().chain(&self.methods).enumerate() {
                if index > 0 {
                    f.line("")?;
                }
                method.fmt(f)?;
            }
            Ok(())
        })
    }
}

/// `enum Name: string` and a case per value.
pub struct Enum {
    /// Its doc block.
    pub doc: DocBlock,
    /// Its name.
    pub name: String,
    /// Each case's name and value.
    pub cases: Vec<(String, String)>,
}

impl Enum {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.doc.fmt(f)?;
        f.braced(&format!("enum {}: string", self.name), |f| {
            self.cases
                .iter()
                .try_for_each(|(name, value)| f.line(&format!("case {name} = {};", quote(value))))
        })
    }
}

/// `const NAME = value;`.
pub struct Const {
    /// Its doc block.
    pub doc: DocBlock,
    /// Its name.
    pub name: String,
    /// Its value.
    pub value: Value,
}

impl Const {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.doc.fmt(f)?;
        let value = self
            .value
            .clone()
            .prefix(&format!("const {} = ", self.name))
            .suffix(";");
        f.value(&value)
    }
}

/// A declaration of a [`File`].
pub enum Item {
    /// A class.
    Class(Class),
    /// A backed enum.
    Enum(Enum),
    /// A constant.
    Const(Const),
    /// A function.
    Function(Method),
}

impl Item {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Class(item) => item.fmt(f),
            Self::Enum(item) => item.fmt(f),
            Self::Const(item) => item.fmt(f),
            Self::Function(item) => item.fmt(f),
        }
    }
}

/// `use Name;`: a class imported into the namespace.
pub struct Use(pub String);

/// A file: `<?php`, its marker, `declare(strict_types=1);`, its namespace,
/// imports and declarations.
pub struct File {
    /// The generated-code marker.
    pub marker: String,
    /// The namespace.
    pub namespace: String,
    /// What it imports.
    pub uses: Vec<Use>,
    /// Its declarations.
    pub items: Vec<Item>,
}

impl File {
    /// Writes it: declarations a blank line apart.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.line(&format!(
            "<?php\n\n// {}\n\ndeclare(strict_types=1);\n\nnamespace {};",
            self.marker, self.namespace
        ))?;
        if !self.uses.is_empty() {
            f.line("")?;
            self.uses
                .iter()
                .try_for_each(|Use(name)| f.line(&format!("use {name};")))?;
        }
        for item in &self.items {
            f.line("")?;
            item.fmt(f)?;
        }
        Ok(())
    }
}

/// PHP's reserved words and type names, lower case: no class, function or
/// constant takes one (the comparison ignores case).
const RESERVED: &str = "\
    __halt_compiler abstract and array as bool break callable case catch class clone const \
    continue declare default die do echo else elseif empty enddeclare endfor endforeach \
    endif endswitch endwhile enum eval exit extends false final finally float fn for foreach \
    function global goto if implements include include_once instanceof insteadof int \
    interface isset iterable list match mixed namespace never new null numeric object or \
    parent print private protected public readonly require require_once resource return self \
    static string switch throw trait true try unset use var void while xor yield";

/// `name`, `_` after it when it is reserved.
fn escape(name: String) -> String {
    if RESERVED
        .split(' ')
        .any(|word| word.eq_ignore_ascii_case(&name))
    {
        name + "_"
    } else {
        name
    }
}

/// `text` as a single-quoted literal.
#[must_use]
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A spec name as PHP writes it.
pub struct PhpName<'a>(pub &'a str);

impl PhpName<'_> {
    /// A class or enum: `PetList`; `List_` for a reserved one.
    #[must_use]
    pub fn class(&self) -> String {
        escape(pascal(self.0))
    }

    /// A property or variable: `nextCursor`; `this_`, as `$this` is taken.
    #[must_use]
    pub fn variable(&self) -> String {
        let name = camel(self.0);
        if name == "this" { name + "_" } else { name }
    }

    /// A function: `walletGet`; `list_` for a reserved one.
    #[must_use]
    pub fn function(&self) -> String {
        escape(camel(self.0))
    }

    /// An enum case: `NoData`; `Class_`, as `class` is taken.
    #[must_use]
    pub fn case(&self) -> String {
        let name = pascal(self.0);
        if name.eq_ignore_ascii_case("class") {
            name + "_"
        } else {
            name
        }
    }

    /// A constant: `WALLET_GET`.
    #[must_use]
    pub fn constant(&self) -> String {
        escape(snake(self.0).to_uppercase())
    }
}

/// A spec type as PHP writes it: its native type (`Display`), and its
/// PHPDoc type ([`PhpType::doc`]).
pub struct PhpType<'a> {
    /// The type.
    pub ty: &'a Type,
    /// Every model, to write a union as its variants.
    pub models: &'a Models<'a>,
}

impl<'a> PhpType<'a> {
    /// `ty`, written against `models`.
    #[must_use]
    pub fn new(ty: &'a Type, models: &'a Models<'a>) -> Self {
        Self { ty, models }
    }

    /// `of`, written against the same models.
    fn of(&self, of: &'a Type) -> Self {
        Self::new(of, self.models)
    }

    /// The variants of the union `ty` names, if it does.
    fn variants(&self) -> Option<&'a [Type]> {
        match self.ty {
            Type::Named(name) => match self.models.get(name.as_str()) {
                Some(Model::Union { variants, .. }) => Some(variants),
                _ => None,
            },
            _ => None,
        }
    }

    /// Its PHPDoc type: `list<Pet>`, `array<string, int>|null`.
    #[must_use]
    pub fn doc(&self) -> String {
        match self.ty {
            Type::Array(item) => format!("list<{}>", self.of(item).doc()),
            Type::Map(value) => format!("array<string, {}>", self.of(value).doc()),
            Type::Optional(inner) => nullable(&self.of(inner).doc()),
            Type::Named(_) => match self.variants() {
                Some(variants) => joined(variants.iter().map(|of| self.of(of).doc())),
                None => self.to_string(),
            },
            _ => self.to_string(),
        }
    }

    /// Its PHPDoc type when the native one does not say it all; else empty.
    #[must_use]
    pub fn doc_only(&self) -> String {
        let doc = self.doc();
        let native = self.to_string();
        let native = native.strip_prefix('?').map_or(native.clone(), nullable);
        if doc == native { String::new() } else { doc }
    }
}

impl fmt::Display for PhpType<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            Type::String => out.write_str("string"),
            Type::Int | Type::Int32 => out.write_str("int"),
            Type::Float => out.write_str("float"),
            Type::Bool => out.write_str("bool"),
            Type::Any => out.write_str("mixed"),
            Type::Array(_) | Type::Map(_) => out.write_str("array"),
            Type::Named(name) => match self.variants() {
                Some(variants) => {
                    out.write_str(&joined(variants.iter().map(|of| self.of(of).to_string())))
                }
                None => out.write_str(name),
            },
            Type::Optional(inner) => {
                let inner = self.of(inner).to_string();
                if inner == "mixed" || inner.contains('|') {
                    out.write_str(&nullable(&inner))
                } else {
                    write!(out, "?{inner}")
                }
            }
        }
    }
}

/// `types` as one union, each once.
fn joined(types: impl Iterator<Item = String>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for ty in types.flat_map(|ty| ty.split('|').map(str::to_owned).collect::<Vec<_>>()) {
        if !seen.contains(&ty) {
            seen.push(ty);
        }
    }
    seen.join("|")
}

/// `ty` or `null`: `mixed` already is.
fn nullable(ty: &str) -> String {
    if ty == "mixed" || ty.split('|').any(|one| one == "null") {
        ty.to_owned()
    } else {
        format!("{ty}|null")
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

display!(DocBlock, Method, Class, Enum, Const, File);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_promote_their_properties() {
        let mut pet = Class::readonly("Pet", DocBlock::new("A pet."));
        pet.property("name", "?string", "", "null", "What it answers to.");
        pet.property("tags", "array", "list<string>", "", "");
        pet.implements.push("Request".into());
        let mut get = Method::public("tags", &[], "array", DocBlock::default());
        get.line("return $this->tags;");
        pet.methods.push(get);
        let text = "/**\n * A pet.\n */\nfinal readonly class Pet implements Request\n{\n    /**\n     * @param list<string> $tags\n     * @param ?string $name What it answers to.\n     */\n    public function __construct(public array $tags, public ?string $name = null) {}\n\n    public function tags(): array\n    {\n        return $this->tags;\n    }\n}\n";
        assert_eq!(pet.to_string(), text);
    }

    #[test]
    fn enums_consts_functions_and_files() {
        let kind = Enum {
            doc: DocBlock::default(),
            name: "Kind".into(),
            cases: vec![("NoData".into(), "no_data".into())],
        };
        assert_eq!(
            kind.to_string(),
            "enum Kind: string\n{\n    case NoData = 'no_data';\n}\n"
        );
        let mut func = Method::function("f", &["Client $client"], "void", DocBlock::new("F."));
        func.push(Body::If(vec![
            ("$a".into(), vec![Body::Line("x();".into())]),
            ("$b".into(), vec![Body::Line("y();".into())]),
        ]));
        assert_eq!(
            func.to_string(),
            "/**\n * F.\n */\nfunction f(Client $client): void\n{\n    if ($a) {\n        x();\n    } elseif ($b) {\n        y();\n    }\n}\n"
        );
        let value = Value::call("new Operation", vec![Value::text("id: 'x'")]);
        let file = File {
            marker: "M.".into(),
            namespace: "Mini".into(),
            uses: vec![Use("Generator".into())],
            items: vec![Item::Const(Const {
                doc: DocBlock::default(),
                name: "X".into(),
                value,
            })],
        };
        assert_eq!(
            file.to_string(),
            "<?php\n\n// M.\n\ndeclare(strict_types=1);\n\nnamespace Mini;\n\nuse Generator;\n\nconst X = new Operation(id: 'x');\n"
        );
    }

    #[test]
    fn long_lists_break() {
        let items = (0..12)
            .map(|n| Value::text(format!("argument{n}")))
            .collect();
        let text = Value::call("f", items).suffix(";").render(0, 0);
        assert!(text.starts_with("f(\n    argument0,\n"), "{text}");
        assert!(text.ends_with("    argument11,\n);"), "{text}");
        let mut long = Method::function("g", &[], "void", DocBlock::default());
        long.params = (0..9).map(|n| format!("string $parameter{n}")).collect();
        assert!(
            long.to_string()
                .contains("    string $parameter8,\n): void {}\n")
        );
    }

    #[test]
    fn names_and_types() {
        assert_eq!(PhpName("list").class(), "List_");
        assert_eq!(PhpName("list").function(), "list_");
        assert_eq!(PhpName("next_cursor").variable(), "nextCursor");
        assert_eq!(PhpName("this").variable(), "this_");
        assert_eq!(PhpName("class").case(), "Class_");
        assert_eq!(PhpName("1h").case(), "V1h");
        assert_eq!(PhpName("wallet_get").constant(), "WALLET_GET");
        assert_eq!(quote("a'b\\c"), "'a\\'b\\\\c'");
        let union = Model::Union {
            name: "Animal".into(),
            doc: String::new(),
            variants: vec![Type::Named("Pet".into()), Type::Named("Robot".into())],
        };
        let models = Models::from([("Animal", &union)]);
        let animals = Type::Optional(Box::new(Type::Array(Box::new(Type::Named(
            "Animal".into(),
        )))));
        assert_eq!(PhpType::new(&animals, &models).to_string(), "?array");
        assert_eq!(
            PhpType::new(&animals, &models).doc(),
            "list<Pet|Robot>|null"
        );
        let animal = Type::Optional(Box::new(Type::Named("Animal".into())));
        assert_eq!(PhpType::new(&animal, &models).to_string(), "Pet|Robot|null");
        assert_eq!(PhpType::new(&animal, &models).doc_only(), "");
        let any = Type::Optional(Box::new(Type::Any));
        assert_eq!(PhpType::new(&any, &models).to_string(), "mixed");
    }
}
