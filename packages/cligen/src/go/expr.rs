//! Go syntax, as gofmt writes it, in the shape of the `codegen` crate: small
//! structs with public fields and a builder method or two, each writing
//! itself through a [`Formatter`] that tracks indentation (tabs), its
//! [`Formatter::block`] writing `{`, the body one level deeper, `}`.
//! Consecutive fields and constants are aligned as gofmt aligns them.

use std::fmt::{self, Write};

use crate::name::snake;
use crate::out::quote;
use crate::spec::Type;

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
                write!(self.dst, "{}", "\t".repeat(self.level))?;
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

    /// `text` as `//` comment lines; nothing for no text.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn comment(&mut self, text: &str) -> fmt::Result {
        for line in text.lines() {
            let line = line.trim_end();
            self.line(&if line.is_empty() {
                "//".to_owned()
            } else {
                format!("// {line}")
            })?;
        }
        Ok(())
    }

    /// Rows of cells, each column as wide as its widest cell within a run of
    /// rows that have a cell after it, as gofmt's tabwriter aligns them; a
    /// comment line ends a run.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn aligned(&mut self, rows: &[(&str, Vec<String>)]) -> fmt::Result {
        let mut widths: Vec<Vec<usize>> =
            rows.iter().map(|(_, cells)| vec![0; cells.len()]).collect();
        let columns = rows.iter().map(|(_, cells)| cells.len()).max().unwrap_or(0);
        for column in 0..columns {
            let has = |row: usize| rows[row].1.len() > column + 1;
            let mut start = 0;
            while start < rows.len() {
                let mut end = start + 1;
                while has(start) && end < rows.len() && has(end) && rows[end].0.is_empty() {
                    end += 1;
                }
                let width = (start..end)
                    .filter(|row| has(*row))
                    .map(|row| rows[row].1[column].chars().count())
                    .max()
                    .unwrap_or(0);
                widths[start..end]
                    .iter_mut()
                    .filter_map(|row| row.get_mut(column))
                    .for_each(|cell| *cell = width);
                start = end;
            }
        }
        for ((doc, cells), widths) in rows.iter().zip(widths) {
            self.comment(doc)?;
            let line: Vec<String> = cells
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:width$}"))
                .collect();
            self.line(line.join(" ").trim_end())?;
        }
        Ok(())
    }
}

/// A statement of a [`Block`].
pub enum Body {
    /// One line.
    Line(String),
    /// A nested block.
    Block(Block),
    /// A `switch`.
    Switch(Switch),
}

impl Body {
    /// Writes it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(line) => f.line(line),
            Self::Block(block) => block.fmt(f),
            Self::Switch(switch) => switch.fmt(f),
        }
    }
}

/// `head { body }`: an `if`, a `for`.
pub struct Block {
    /// What comes before `{`: `if p.Limit != nil`.
    pub head: String,
    /// Its statements.
    pub body: Vec<Body>,
}

impl Block {
    /// An empty block after `head`.
    #[must_use]
    pub fn new(head: &str) -> Self {
        Self {
            head: head.to_owned(),
            body: Vec::new(),
        }
    }

    /// Adds a line.
    pub fn line(&mut self, line: impl Into<String>) -> &mut Self {
        self.body.push(Body::Line(line.into()));
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.block(&self.head, |f| {
            self.body.iter().try_for_each(|body| body.fmt(f))
        })
    }
}

/// `switch on { case value: … }`.
pub struct Switch {
    /// What it switches on.
    pub on: String,
    /// Each case's value and statements.
    pub cases: Vec<(String, Vec<String>)>,
}

impl Switch {
    /// Writes it: `case` at the `switch`'s level, as gofmt writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.line(&format!("switch {} {{", self.on))?;
        for (value, body) in &self.cases {
            f.line(&format!("case {value}:"))?;
            f.level += 1;
            body.iter().try_for_each(|line| f.line(line))?;
            f.level -= 1;
        }
        f.line("}")
    }
}

/// A struct field: `Name Type \`tag\``, its comment above it; an embedded
/// one has no name.
pub struct Field {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: String,
    /// Its tag, without backquotes; empty: none.
    pub tag: String,
}

/// `type Name struct { fields }`.
pub struct Struct {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Its fields.
    pub fields: Vec<Field>,
}

impl Struct {
    /// An empty struct `name`, documented with `doc`.
    #[must_use]
    pub fn new(name: &str, doc: &str) -> Self {
        Self {
            doc: doc.to_owned(),
            name: name.to_owned(),
            fields: Vec::new(),
        }
    }

    /// Adds a field.
    pub fn field(&mut self, name: &str, ty: &str, tag: &str, doc: &str) -> &mut Self {
        self.fields.push(Field {
            doc: doc.to_owned(),
            name: name.to_owned(),
            ty: ty.to_owned(),
            tag: tag.to_owned(),
        });
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.doc)?;
        if self.fields.is_empty() {
            return f.line(&format!("type {} struct{{}}", self.name));
        }
        let rows: Vec<(&str, Vec<String>)> = self
            .fields
            .iter()
            .map(|field| {
                let mut cells = if field.name.is_empty() {
                    vec![field.ty.clone()]
                } else {
                    vec![field.name.clone(), field.ty.clone()]
                };
                if !field.tag.is_empty() {
                    cells.push(format!("`{}`", field.tag));
                }
                (field.doc.as_str(), cells)
            })
            .collect();
        f.block(&format!("type {} struct", self.name), |f| f.aligned(&rows))
    }
}

/// `type Name string` and a constant per value.
pub struct Enum {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// Each constant's name and value.
    pub values: Vec<(String, String)>,
}

impl Enum {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.doc)?;
        f.line(&format!("type {} string\n\nconst (", self.name))?;
        let rows: Vec<(&str, Vec<String>)> = self
            .values
            .iter()
            .map(|(name, value)| {
                (
                    "",
                    vec![
                        name.clone(),
                        self.name.clone(),
                        format!("= {}", quote(value)),
                    ],
                )
            })
            .collect();
        f.level += 1;
        f.aligned(&rows)?;
        f.level -= 1;
        f.line(")")
    }
}

/// `var name = Type{ fields }`, one field per line, values aligned.
pub struct Var {
    /// Its comment.
    pub doc: String,
    /// Its name.
    pub name: String,
    /// The composite literal's type.
    pub ty: String,
    /// Each field's name and value.
    pub fields: Vec<(String, String)>,
}

impl Var {
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.doc)?;
        let rows: Vec<(&str, Vec<String>)> = self
            .fields
            .iter()
            .map(|(name, value)| ("", vec![format!("{name}:"), format!("{value},")]))
            .collect();
        f.line(&format!("var {} = {}{{", self.name, self.ty))?;
        f.level += 1;
        f.aligned(&rows)?;
        f.level -= 1;
        f.line("}")
    }
}

/// `func (receiver) Name(params) results { body }`.
pub struct Func {
    /// Its comment.
    pub doc: String,
    /// Its receiver: `p ReadWalletParams`; empty: a function.
    pub receiver: String,
    /// Its name.
    pub name: String,
    /// Its parameters, as written.
    pub params: String,
    /// What it returns, as written; empty: nothing.
    pub results: String,
    /// Its statements.
    pub body: Vec<Body>,
}

impl Func {
    /// The function `name(params) results`, documented with `doc`, of no
    /// statements yet.
    #[must_use]
    pub fn new(name: &str, params: &str, results: &str, doc: &str) -> Self {
        let (name, params, results, doc) = (
            name.to_owned(),
            params.to_owned(),
            results.to_owned(),
            doc.to_owned(),
        );
        Self {
            doc,
            receiver: String::new(),
            name,
            params,
            results,
            body: Vec::new(),
        }
    }

    /// Makes it a method of `receiver`.
    #[must_use]
    pub fn receiver(mut self, receiver: &str) -> Self {
        self.receiver = receiver.to_owned();
        self
    }

    /// Adds a line.
    pub fn line(&mut self, line: impl Into<String>) -> &mut Self {
        self.body.push(Body::Line(line.into()));
        self
    }

    /// Adds a block or a switch.
    pub fn push(&mut self, body: Body) -> &mut Self {
        self.body.push(body);
        self
    }

    /// Writes it.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.comment(&self.doc)?;
        let receiver = if self.receiver.is_empty() {
            String::new()
        } else {
            format!("({}) ", self.receiver)
        };
        let results = if self.results.is_empty() {
            String::new()
        } else {
            format!(" {}", self.results)
        };
        let head = format!("func {receiver}{}({}){results}", self.name, self.params);
        f.block(&head, |f| self.body.iter().try_for_each(|body| body.fmt(f)))
    }
}

/// A declaration of a [`File`].
pub enum Item {
    /// A struct.
    Struct(Struct),
    /// A string enum.
    Enum(Enum),
    /// A package variable.
    Var(Var),
    /// A function or method.
    Func(Func),
}

/// A file: its marker, package, imports and declarations.
pub struct File {
    /// The generated-code marker.
    pub marker: String,
    /// The package name.
    pub package: String,
    /// The packages it imports.
    pub imports: Vec<String>,
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
        f.line(&format!("// {}\n\npackage {}", self.marker, self.package))?;
        if !self.imports.is_empty() {
            f.line("\nimport (")?;
            self.imports
                .iter()
                .try_for_each(|import| f.line(&format!("\t{}", quote(import))))?;
            f.line(")")?;
        }
        for item in &self.items {
            f.line("")?;
            match item {
                Item::Struct(item) => item.fmt(f)?,
                Item::Enum(item) => item.fmt(f)?,
                Item::Var(item) => item.fmt(f)?,
                Item::Func(item) => item.fmt(f)?,
            }
        }
        Ok(())
    }
}

/// golint's common initialisms.
const INITIALISMS: [&str; 39] = [
    "ACL", "API", "ASCII", "CPU", "CSS", "DNS", "EOF", "GUID", "HTML", "HTTP", "HTTPS", "ID", "IP",
    "JSON", "LHS", "QPS", "RAM", "RHS", "RPC", "SLA", "SMTP", "SQL", "SSE", "SSH", "TCP", "TLS",
    "TTL", "UDP", "UI", "UID", "URI", "URL", "UTF8", "UUID", "VM", "XML", "XMPP", "XSRF", "XSS",
];

/// A spec type as Go writes it: `[]Wallet`, `*string`, `map[string]int64`.
pub struct GoType<'a>(pub &'a Type);

impl GoType<'_> {
    /// Whether nil is a value of it already (slice, map, raw JSON): an
    /// optional one needs no pointer.
    #[must_use]
    pub fn is_reference(&self) -> bool {
        match self.0 {
            Type::Array(_) | Type::Map(_) | Type::Any => true,
            Type::Optional(inner) => GoType(inner).is_reference(),
            _ => false,
        }
    }

    /// As a field's or parameter's type: a pointer when not `required`,
    /// unless already one or a reference.
    #[must_use]
    pub fn field(&self, required: bool) -> String {
        let written = self.to_string();
        if required || self.is_reference() || written.starts_with('*') {
            written
        } else {
            format!("*{written}")
        }
    }
}

impl fmt::Display for GoType<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Type::String => out.write_str("string"),
            Type::Int => out.write_str("int64"),
            Type::Int32 => out.write_str("int32"),
            Type::Float => out.write_str("float64"),
            Type::Bool => out.write_str("bool"),
            Type::Any => out.write_str("jsonValue"),
            Type::Named(name) => out.write_str(name),
            Type::Array(item) => write!(out, "[]{}", GoType(item)),
            Type::Map(value) => write!(out, "map[string]{}", GoType(value)),
            Type::Optional(inner) if GoType(inner).is_reference() => {
                write!(out, "{}", GoType(inner))
            }
            Type::Optional(inner) => write!(out, "*{}", GoType(inner)),
        }
    }
}

/// A spec name as Go writes it.
pub struct GoName<'a>(pub &'a str);

impl GoName<'_> {
    /// Exported: each word capitalized, an initialism upper case
    /// (`external_id` → `ExternalID`).
    #[must_use]
    pub fn exported(&self) -> String {
        let name: String = snake(self.0)
            .split('_')
            .map(|word| {
                let upper = word.to_uppercase();
                if INITIALISMS.contains(&upper.as_str()) {
                    return upper;
                }
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            })
            .collect();
        if name.starts_with(|c: char| c.is_ascii_digit()) {
            format!("V{name}")
        } else {
            name
        }
    }

    /// Unexported: the exported name, its first word lower case
    /// (`network_endpoints_create` → `networkEndpointsCreate`, `id_get` →
    /// `idGet`).
    #[must_use]
    pub fn unexported(&self) -> String {
        let exported = self.exported();
        let first = crate::name::snake(self.0)
            .split('_')
            .next()
            .unwrap_or_default()
            .len();
        let first = exported
            .char_indices()
            .nth(first)
            .map_or(exported.len(), |(at, _)| at);
        exported[..first].to_lowercase() + &exported[first..]
    }

    /// Its doc comment from `doc`: `Name is …`, the first word lowered when
    /// plain; nothing for no doc.
    #[must_use]
    pub fn is(&self, doc: &str) -> String {
        if doc.is_empty() {
            String::new()
        } else {
            format!("{} is {}", self.0, lower(doc))
        }
    }

    /// Its doc comment from an imperative summary, in the third person:
    /// `Read one wallet` → `ReadWallet reads one wallet.`
    #[must_use]
    pub fn does(&self, summary: &str) -> String {
        let (word, rest) = summary.split_once(' ').unwrap_or((summary, ""));
        let word = lower(word);
        let verb = if ["s", "sh", "ch", "x", "z"]
            .iter()
            .any(|tail| word.ends_with(tail))
        {
            format!("{word}es")
        } else if word.ends_with('y')
            && !["ay", "ey", "oy", "uy"]
                .iter()
                .any(|tail| word.ends_with(tail))
        {
            format!("{}ies", &word[..word.len() - 1])
        } else {
            format!("{word}s")
        };
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!(" {rest}")
        };
        format!("{} {verb}{rest}.", self.0)
    }
}

/// `text` with its first letter lowered when its first word is plain
/// (`The`, not `URL` or `` `x` ``).
fn lower(text: &str) -> String {
    let word = text.split_whitespace().next().unwrap_or_default();
    let mut chars = word.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| !c.is_ascii_uppercase())
        && word.len() > 1;
    if plain {
        text[..1].to_lowercase() + &text[1..]
    } else {
        text.to_owned()
    }
}

/// `Display` through [`fmt`](File::fmt) with a fresh formatter.
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

display!(Block, Switch, Struct, Enum, Var, Func, File);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structs_align_like_gofmt() {
        let mut pet = Struct::new("Pet", "Pet is a pet.");
        pet.field("ID", "string", r#"json:"id""#, "")
            .field("Name", "*string", r#"json:"name""#, "")
            .field("Owner", "Owner", "", "Its owner.");
        let text = "// Pet is a pet.\ntype Pet struct {\n\tID   string  `json:\"id\"`\n\tName *string `json:\"name\"`\n\t// Its owner.\n\tOwner Owner\n}\n";
        assert_eq!(pet.to_string(), text);
        assert_eq!(Struct::new("P", "").to_string(), "type P struct{}\n");
    }

    #[test]
    fn enums_funcs_and_files() {
        let values = vec![
            ("KindCat".into(), "cat".into()),
            ("KindNoData".into(), "no_data".into()),
        ];
        let kind = Enum {
            doc: String::new(),
            name: "Kind".into(),
            values,
        };
        assert_eq!(
            kind.to_string(),
            "type Kind string\n\nconst (\n\tKindCat    Kind = \"cat\"\n\tKindNoData Kind = \"no_data\"\n)\n"
        );
        let mut check = Block::new("if p.A != nil");
        check.line("x()");
        let mut func = Func::new("F", "", "error", "F does.").receiver("p P");
        func.push(Body::Block(check)).line("return nil");
        assert_eq!(
            func.to_string(),
            "// F does.\nfunc (p P) F() error {\n\tif p.A != nil {\n\t\tx()\n\t}\n\treturn nil\n}\n"
        );
        let fields = vec![
            ("ID".into(), "\"x\"".into()),
            ("Method".into(), "\"GET\"".into()),
        ];
        let var = Var {
            doc: String::new(),
            name: "a".into(),
            ty: "Operation".into(),
            fields,
        };
        let file = File {
            marker: "M.".into(),
            package: "x".into(),
            imports: vec!["context".into()],
            items: vec![Item::Var(var)],
        };
        assert_eq!(
            file.to_string(),
            "// M.\n\npackage x\n\nimport (\n\t\"context\"\n)\n\nvar a = Operation{\n\tID:     \"x\",\n\tMethod: \"GET\",\n}\n"
        );
    }
}
