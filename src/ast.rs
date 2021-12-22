use std::io::Read;
use std::convert::TryFrom;
use std::rc::Rc;
use std::mem;
use std::iter;

use linked_hash_map::LinkedHashMap;
use derive_builder::Builder;

use xml::reader::{EventReader, XmlEvent};
use xml::name::OwnedName;
use xml::attribute::OwnedAttribute;
use xml::common::Position;

use serde_json::Value as JsonValue;

use regex::Regex;

#[cfg(feature = "serde")]
use serde::{Serialize, Deserialize};

lazy_static! {
    static ref NUMBER_REGEX: Regex = Regex::new(r"^-?[0-9]+(\.[0-9]*)?([eE][+-]?[0-9]+)?$").unwrap();
    static ref PARAM_FINDER: Regex = Regex::new(r"%'([^']+)'").unwrap();
    static ref NEW_LINE: Regex = Regex::new("\r\n|\r|\n").unwrap();
}

fn clean_newlines(s: &str) -> String {
    NEW_LINE.replace_all(s, "\n").into_owned()
}

#[derive(Debug)]
struct XmlAttr {
    name: String,
    value: String,
}
#[derive(Debug)]
struct Xml {
    name: String,
    text: String,
    attrs: Vec<XmlAttr>,
    children: Vec<Xml>,
}
impl Xml {
    fn get(&self, path: &[&str]) -> Option<&Xml> {
        match path {
            [] => Some(self),
            [first, rest @ ..] => self.children.iter().find(|x| x.name == *first).map(|x| x.get(rest)).flatten(),
        }
    }
    fn attr(&self, name: &str) -> Option<&XmlAttr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}
fn parse_xml_root<R: Read>(xml: &mut EventReader<R>, root_name: OwnedName, root_attrs: Vec<OwnedAttribute>) -> Result<Xml, Error> {
    let mut text = String::new();
    let mut children = vec![];
    loop {
        match xml.next() {
            Ok(XmlEvent::StartElement { name, attributes, .. }) => {
                children.push(parse_xml_root(xml, name, attributes)?);
            }
            Ok(XmlEvent::EndElement { name }) => {
                assert_eq!(name, root_name);
                let attrs = root_attrs.into_iter().map(|a| XmlAttr {
                    name: a.name.local_name,
                    value: a.value,
                }).collect();
                return Ok(Xml { name: root_name.local_name, attrs, children, text: clean_newlines(&text) });
            }
            Ok(XmlEvent::Characters(s)) | Ok(XmlEvent::CData(s)) => text += &s,
            Ok(XmlEvent::Comment(_)) | Ok(XmlEvent::Whitespace(_)) | Ok(XmlEvent::ProcessingInstruction { .. }) => (),
            Ok(x @ XmlEvent::StartDocument { .. }) | Ok(x @ XmlEvent::EndDocument) => panic!("{:?} at pos {:?}", x, xml.position()),
            Err(error) => return Err(Error::InvalidXml { error }),
        }
    }
}

#[derive(Debug)]
pub enum ProjectError {
    NoRoot,
    UnnamedRole,
    ValueNotEvaluated { role: String, sprite: Option<String> },
    InvalidJson { reason: String },
    NoRoleContent { role: String },
    NoStageDef { role: String },

    UnnamedGlobal { role: String },
    GlobalNoValue { role: String, name: String },
    GlobalsWithSameName { role: String, name: String },

    UnnamedField { role: String, sprite: String },
    FieldNoValue { role: String, sprite: String, name: String },
    FieldsWithSameName { role: String, sprite: String, name: String },

    ListItemNoValue { role: String, sprite: String },
    BoolNoValue { role: String, sprite: String },
    BoolUnknownValue { role: String, sprite: String, value: String },
    UnnamedSprite { role: String },

    UnknownBlockMetaType { role: String, sprite: String, meta_type: String },
    BlockWithoutType { role: String, sprite: String },
    BlockChildCount { role: String, sprite: String, block_type: String, needed: usize, got: usize },

    BlockMissingOption { role: String, sprite: String, block_type: String },
    BlockOptionUnknown { role: String, sprite: String, block_type: String, got: String },

    InvalidBoolLiteral { role: String, sprite: String },
    NonConstantUpvar { role: String, sprite: String, block_type: String },
}
#[derive(Debug)]
pub enum Error {
    InvalidXml { error: xml::reader::Error },
    InvalidProject { error: ProjectError },
    NameTransformError { name: String, role: Option<String>, sprite: Option<String> },
    UnknownBlockType { role: String, sprite: String, block_type: String },
    DerefAssignment { role: String, sprite: String },
    UndefinedVariable { role: String, sprite: String, name: String },
    BlockOptionNotConst { role: String, sprite: String, block_type: String },
    BlockOptionNotSelected { role: String, sprite: String, block_type: String },

    GlobalsWithSameTransName { role: String, trans_name: String, names: (String, String) },
    FieldsWithSameTransName { role: String, sprite: String, trans_name: String, names: (String, String) },
    LocalsWithSameTransName { role: String, sprite: String, trans_name: String, names: (String, String) },
}

#[derive(Debug)]
pub enum SymbolError {
    NameTransformError { name: String },
    ConflictingTrans { trans_name: String, names: (String, String) }
}

struct SymbolTable<'a> {
    parser: &'a Parser,
    orig_to_def: LinkedHashMap<String, VariableDef>,
    trans_to_orig: LinkedHashMap<String, String>,
}
impl<'a> SymbolTable<'a> {
    fn new(parser: &'a Parser) -> Self {
        Self { parser, orig_to_def: Default::default(), trans_to_orig: Default::default() }
    }
    fn transform_name(&self, name: &str) -> Result<String, SymbolError> {
        match self.parser.name_transformer.as_ref()(name) {
            Ok(v) => Ok(v),
            Err(()) => Err(SymbolError::NameTransformError { name: name.into() }),
        }
    }
    /// Defines a new symbol or replaces an existing definition.
    /// Fails if the name cannot be properly transformed or the transformed name already exists.
    /// On success, returns the previous definition (if one existed).
    /// On failure, the symbol table is not modified, and an error context object is returned.
    fn define(&mut self, name: String, value: Value) -> Result<Option<VariableDef>, SymbolError> {
        let trans_name = self.transform_name(&name)?;
        let entry = VariableDef { name: name.clone(), trans_name: trans_name.clone(), value };
        if let Some(orig) = self.trans_to_orig.get(&trans_name) {
            let def = self.orig_to_def.get(orig).unwrap();
            return Err(SymbolError::ConflictingTrans { trans_name, names: (def.name.clone(), name) });
        }

        self.trans_to_orig.insert(name.clone(), trans_name);
        Ok(self.orig_to_def.insert(name, entry))
    }
    /// Returns the definition of the given variable if it exists.
    fn get(&self, name: &str) -> Option<&VariableDef> {
        self.orig_to_def.get(name)
    }
    fn into_defs(self) -> Vec<VariableDef> {
        self.orig_to_def.into_iter().map(|x| x.1).collect()
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Project {
    pub name: String,
    pub roles: Vec<Role>,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Role {
    pub name: String,
    pub notes: String,
    pub globals: Vec<VariableDef>,
    pub sprites: Vec<Sprite>,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Sprite {
    pub name: String,
    pub fields: Vec<VariableDef>,
    pub scripts: Vec<Script>,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct VariableDef {
    pub name: String,
    pub trans_name: String,
    pub value: Value,
}
impl VariableDef {
    fn ref_at(&self, location: VarLocation) -> VariableRef {
        VariableRef { name: self.name.clone(), trans_name: self.trans_name.clone(), location }
    }
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct VariableRef {
    pub name: String,
    pub trans_name: String,
    pub location: VarLocation,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum VarLocation {
    Global, Field, Local,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Script {
    pub hat: Option<Hat>,
    pub stmts: Vec<Stmt>,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Hat {
    OnFlag { comment: Option<String> },
    OnKey { key: String, comment: Option<String> },
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Stmt {
    /// Assign the given value to each of the listed variables (afterwards, they should all be ref-eq).
    Assign { vars: Vec<VariableRef>, value: Expr, comment: Option<String> },
    AddAssign { var: VariableRef, value: Expr, comment: Option<String> },

    Warp { stmts: Vec<Stmt>, comment: Option<String> },

    InfLoop { stmts: Vec<Stmt>, comment: Option<String> },
    ForeachLoop { var: VariableRef, items: Expr, stmts: Vec<Stmt>, comment: Option<String> },
    ForLoop { var: VariableRef, first: Expr, last: Expr, stmts: Vec<Stmt>, comment: Option<String> },
    UntilLoop { condition: Expr, stmts: Vec<Stmt>, comment: Option<String> },
    Repeat { times: Expr, stmts: Vec<Stmt>, comment: Option<String> },

    If { condition: Expr, then: Vec<Stmt>, comment: Option<String> },
    IfElse { condition: Expr, then: Vec<Stmt>, otherwise: Vec<Stmt>, comment: Option<String> },

    Push { list: Expr, value: Expr, comment: Option<String> },
    InsertAt { list: Expr, value: Expr, index: Expr, comment: Option<String> },
    InsertAtRand { list: Expr, value: Expr, comment: Option<String> },

    Pop { list: Expr, comment: Option<String> },
    RemoveAt { list: Expr, index: Expr, comment: Option<String> },
    RemoveAll { list: Expr, comment: Option<String> },

    IndexAssign { list: Expr, value: Expr, index: Expr, comment: Option<String> },
    RandIndexAssign { list: Expr, value: Expr, comment: Option<String> },
    LastIndexAssign { list: Expr, value: Expr, comment: Option<String> },

    Return { value: Expr, comment: Option<String> },

    Sleep { seconds: Expr, comment: Option<String> },
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Value {
    Bool(bool),
    Number(f64),
    String(String),
    List(Vec<Value>),
    Constant(Constant),
}

impl From<f64> for Value { fn from(v: f64) -> Value { Value::Number(v) } }
impl From<&str> for Value { fn from(v: &str) -> Value { Value::String(v.into()) } }
impl From<bool> for Value { fn from(v: bool) -> Value { Value::Bool(v) } }
impl From<String> for Value { fn from(v: String) -> Value { Value::String(v) } }
impl From<Constant> for Value { fn from(v: Constant) -> Value { Value::Constant(v) } }
impl From<Vec<Value>> for Value { fn from(v: Vec<Value>) -> Value { Value::List(v) } }

impl TryFrom<JsonValue> for Value {
    type Error = Error;
    fn try_from(val: JsonValue) -> Result<Value, Self::Error> {
        Ok(match val {
            JsonValue::String(v) => Value::String(v),
            JsonValue::Bool(v) => Value::Bool(v),
            JsonValue::Array(vals) => {
                let mut res = Vec::with_capacity(vals.len());
                for val in vals { res.push(Value::try_from(val)?) }
                Value::List(res)
            }
            JsonValue::Number(v) => match v.as_f64() {
                Some(v) => Value::Number(v),
                None => return Err(Error::InvalidProject { error: ProjectError::InvalidJson { reason: format!("failed to convert {} to f64", v) } }),
            }
            JsonValue::Object(_) => return Err(Error::InvalidProject { error: ProjectError::InvalidJson { reason: format!("got object: {}", val) } }),
            JsonValue::Null => return Err(Error::InvalidProject { error: ProjectError::InvalidJson { reason: format!("got null") } }),
        })
    }
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Constant {
    E, Pi,
}
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Expr {
    Value(Value),
    Variable { var: VariableRef, comment: Option<String> },

    Add { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Sub { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Mul { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Div { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    /// Mathematical modulus (not remainder!). For instance, `-1 mod 7 == 6`.
    Mod { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },

    Pow { base: Box<Expr>, power: Box<Expr>, comment: Option<String> },
    Log { value: Box<Expr>, base: Box<Expr>, comment: Option<String> },

    /// Short-circuiting logical `or`.
    And { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    /// Short-circuiting logical `and`.
    Or { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    /// Lazily-evaluated conditional expression. Returns `then` if `condition` is true, otherwise `otherwise`.
    Conditional { condition: Box<Expr>, then: Box<Expr>, otherwise: Box<Expr>, comment: Option<String> },

    RefEq { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Eq { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Less { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },
    Greater { left: Box<Expr>, right: Box<Expr>, comment: Option<String> },

    /// Get a random number between `a` and `b` (inclusive).
    /// There are no ordering guarantees (swapping `a` and `b` is equivalent).
    /// If both values are integers, the result is an integer, otherwise continuous floats are returned.
    RandInclusive { a: Box<Expr>, b: Box<Expr>, comment: Option<String> },
    /// Get a list of all the numbers starting at `start` and stepping towards `stop` (by `+1` or `-1`), but not going past `stop`.
    RangeInclusive { start: Box<Expr>, stop: Box<Expr>, comment: Option<String> },

    MakeList { values: Vec<Expr>, comment: Option<String> },
    Listcat { lists: Vec<Expr>, comment: Option<String> },
    Listlen { value: Box<Expr>, comment: Option<String> },
    /// Return a shallow copy of the sublist on the inclusive range `[from, to]`.
    /// If one of the bounds is not given, go all the way to that end.
    /// Lists are 1-indexed by default.
    ListSlice { value: Box<Expr>, from: Option<Box<Expr>>, to: Option<Box<Expr>>, comment: Option<String> },
    /// Returns the (1-based) index of value in the list, or 0 if not present.
    ListFind { list: Box<Expr>, value: Box<Expr>, comment: Option<String> },

    ListIndex { list: Box<Expr>, index: Box<Expr>, comment: Option<String> },
    ListRandIndex { list: Box<Expr>, comment: Option<String> },
    ListLastIndex { list: Box<Expr>, comment: Option<String> },

    Strcat { values: Vec<Expr>, comment: Option<String> },
    /// String length in terms of unicode code points (not bytes or grapheme clusters!).
    Strlen { value: Box<Expr>, comment: Option<String> },

    /// Convert a unicode code point into a 1-character string.
    UnicodeToChar { value: Box<Expr>, comment: Option<String> },
    /// Convert a 1-character string into its unicode code point.
    CharToUnicode { value: Box<Expr>, comment: Option<String> },

    Not { value: Box<Expr>, comment: Option<String> },
    Neg { value: Box<Expr>, comment: Option<String> },
    Abs { value: Box<Expr>, comment: Option<String> },
    Sqrt { value: Box<Expr>, comment: Option<String> },

    Floor { value: Box<Expr>, comment: Option<String> },
    Ceil { value: Box<Expr>, comment: Option<String> },
    Round { value: Box<Expr>, comment: Option<String> },

    Sin { value: Box<Expr>, comment: Option<String> },
    Cos { value: Box<Expr>, comment: Option<String> },
    Tan { value: Box<Expr>, comment: Option<String> },

    Asin { value: Box<Expr>, comment: Option<String> },
    Acos { value: Box<Expr>, comment: Option<String> },
    Atan { value: Box<Expr>, comment: Option<String> },
}
impl<T: Into<Value>> From<T> for Expr { fn from(v: T) -> Expr { Expr::Value(v.into()) } }

macro_rules! check_children_get_comment {
    ($self:ident, $expr:ident, $s:expr => $req:literal) => {{
        let s = $s;
        #[allow(unused_comparisons)]
        if $expr.children.len() < $req {
            return Err(Error::InvalidProject { error: ProjectError::BlockChildCount { role: $self.role.name.clone(), sprite: $self.sprite.name.clone(), block_type: s.into(), needed: $req, got: $expr.children.len() } });
        }
        match $expr.children.get($req) {
            Some(comment) => if comment.name == "comment" { Some(clean_newlines(&comment.text)) } else { None },
            None => None,
        }
    }}
}
macro_rules! binary_op {
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })? : $left:ident, $right:ident) => {{
        let comment = check_children_get_comment!($self, $expr, $s => 2);
        let $left = $self.parse_expr(&$expr.children[0])?.into();
        let $right = $self.parse_expr(&$expr.children[1])?.into();
        $res { $left, $right, comment, $( $($field : $value),* )? }
    }};
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })?) => {
        binary_op! { $self, $expr, $s => $res $({ $($field : $value),* })? : left, right }
    }
}
macro_rules! unary_op {
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })? : $val:ident) => {{
        let comment = check_children_get_comment!($self, $expr, $s => 1);
        let $val = $self.parse_expr(&$expr.children[0])?.into();
        $res { $val, comment, $( $($field : $value),* )? }
    }};
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })? ) => {
        unary_op! { $self, $expr, $s => $res $({ $($field : $value),* })? : value }
    }
}
macro_rules! variadic_op {
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })? : $val:ident) => {{
        let comment = check_children_get_comment!($self, $expr, $s => 1);
        let mut $val = vec![];
        for item in $expr.children[0].children.iter() {
            $val.push($self.parse_expr(item)?);
        }
        $res { $val, comment, $( $($field : $value),* )? }
    }};
    ($self:ident, $expr:ident, $s:expr => $res:path $({ $($field:ident : $value:expr),*$(,)? })?) => {
        variadic_op! { $self, $expr, $s => $res $({ $($field : $value),* })? : values }
    }
}

struct ScriptInfo<'a> {
    role: &'a RoleInfo<'a>,
    sprite: &'a SpriteInfo<'a>,
    locals: SymbolTable<'a>,
}
impl<'a> ScriptInfo<'a> {
    fn new(sprite: &'a SpriteInfo) -> Self {
        Self { role: sprite.role, sprite, locals: SymbolTable::new(sprite.parser) }
    }
    fn parse(&mut self, script: &Xml) -> Result<Script, Error> {
        if script.children.is_empty() { return Ok(Script { hat: None, stmts: vec![] }) }

        let (hat, stmts_xml) = match self.parse_hat(&script.children[0])? {
            None => (None, script.children.as_slice()),
            Some(hat) => (Some(hat), &script.children[1..]),
        };

        let mut stmts = vec![];
        for stmt in stmts_xml {
            match stmt.name.as_str() {
                "block" => stmts.push(self.parse_block(stmt)?),
                x => return Err(Error::InvalidProject { error: ProjectError::UnknownBlockMetaType { role: self.role.name.clone(), sprite: self.sprite.name.clone(), meta_type: x.to_owned() } }),
            }
        }
        Ok(Script { hat, stmts })
    }
    fn parse_hat(&self, stmt: &Xml) -> Result<Option<Hat>, Error> {
        let s = match stmt.attr("s") {
            None => return Err(Error::InvalidProject { error: ProjectError::BlockWithoutType { role: self.role.name.clone(), sprite: self.sprite.name.clone() } }),
            Some(v) => v.value.as_str(),
        };
        Ok(Some(match s {
            "receiveGo" => {
                let comment = check_children_get_comment!(self, stmt, s => 0);
                Hat::OnFlag { comment }
            }
            "receiveKey" => {
                let comment = check_children_get_comment!(self, stmt, s => 1);
                let key = match stmt.children[0].get(&["option"]) {
                    None => return Err(Error::InvalidProject { error: ProjectError::BlockMissingOption { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() } }),
                    Some(k) => {
                        if k.children.len() != 0 { return Err(Error::BlockOptionNotConst { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }) }
                        k.text.clone()
                    }
                };
                if key == "" { return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }) }
                Hat::OnKey { key, comment }
            }
            _ => return Ok(None),
        }))
    }
    fn parse_block(&mut self, stmt: &Xml) -> Result<Stmt, Error> {
        macro_rules! define_local_and_ref {
            ($name:expr, $value:expr) => {{
                let name = $name;
                match self.locals.define(name.clone(), $value) {
                    Ok(_) => (), // redefining locals is fine
                    Err(SymbolError::NameTransformError { name }) => return Err(Error::NameTransformError { name, role: Some(self.role.name.clone()), sprite: Some(self.sprite.name.clone()) }),
                    Err(SymbolError::ConflictingTrans { trans_name, names }) => return Err(Error::LocalsWithSameTransName { role: self.role.name.clone(), sprite: self.sprite.name.clone(), trans_name, names }),
                }
                self.locals.get(&name).unwrap().ref_at(VarLocation::Local)
            }}
        }
        let s = match stmt.attr("s") {
            None => return Err(Error::InvalidProject { error: ProjectError::BlockWithoutType { role: self.role.name.clone(), sprite: self.sprite.name.clone() } }),
            Some(v) => v.value.as_str(),
        };
        Ok(match s {
            "doDeclareVariables" => {
                let comment = check_children_get_comment!(self, stmt, s => 1);
                let mut vars = vec![];
                for var in stmt.children[0].children.iter() {
                    vars.push(define_local_and_ref!(var.text.clone(), 0f64.into()));
                }
                Stmt::Assign { vars, value: 0f64.into(), comment }
            }
            "doSetVar" | "doChangeVar" => {
                let comment = check_children_get_comment!(self, stmt, s => 2);
                let var = match stmt.children[0].name.as_str() {
                    "l" => self.reference_var(stmt.children[0].text.clone())?,
                    _ => return Err(Error::DerefAssignment { role: self.role.name.clone(), sprite: self.sprite.name.clone() }),
                };
                let value = self.parse_expr(&stmt.children[1])?;
                match s {
                    "doSetVar" => Stmt::Assign { vars: vec![var], value, comment },
                    "doChangeVar" => Stmt::AddAssign { var, value, comment },
                    _ => unreachable!(),
                }
            }
            "doFor" => {
                let comment = check_children_get_comment!(self, stmt, s => 4);
                let var = match stmt.children[0].name.as_str() {
                    "l" => stmt.children[0].text.as_str(),
                    _ => return Err(Error::InvalidProject { error: ProjectError::NonConstantUpvar { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() } }),
                };
                let first = self.parse_expr(&stmt.children[1])?;
                let last = self.parse_expr(&stmt.children[2])?;
                let stmts = self.parse(&stmt.children[3])?.stmts;

                let var = define_local_and_ref!(var.to_owned(), 0f64.into());
                Stmt::ForLoop { var, first, last, stmts, comment }
            }
            "doForEach" => {
                let comment = check_children_get_comment!(self, stmt, s => 3);
                let var = match stmt.children[0].name.as_str() {
                    "l" => stmt.children[0].text.as_str(),
                    _ => return Err(Error::InvalidProject { error: ProjectError::NonConstantUpvar { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() } }),
                };
                let items = self.parse_expr(&stmt.children[1])?;
                let stmts = self.parse(&stmt.children[2])?.stmts;

                let var = define_local_and_ref!(var.to_owned(), 0f64.into());
                Stmt::ForeachLoop { var, items, stmts, comment }
            }
            "doRepeat" | "doUntil" | "doIf" => {
                let comment = check_children_get_comment!(self, stmt, s => 2);
                let expr = self.parse_expr(&stmt.children[0])?;
                let stmts = self.parse(&stmt.children[1])?.stmts;
                match s {
                    "doRepeat" => Stmt::Repeat { times: expr, stmts, comment },
                    "doUntil" => Stmt::UntilLoop { condition: expr, stmts, comment },
                    "doIf" => Stmt::If { condition: expr, then: stmts, comment },
                    _ => unreachable!(),
                }
            }
            "doForever" => {
                let comment = check_children_get_comment!(self, stmt, s => 1);
                let stmts = self.parse(&stmt.children[0])?.stmts;
                Stmt::InfLoop { stmts, comment }
            }
            "doIfElse" => {
                let comment = check_children_get_comment!(self, stmt, s => 3);
                let condition = self.parse_expr(&stmt.children[0])?;
                let then = self.parse(&stmt.children[1])?.stmts;
                let otherwise = self.parse(&stmt.children[2])?.stmts;
                Stmt::IfElse { condition, then, otherwise, comment }
            }
            "doWarp" => {
                let comment = check_children_get_comment!(self, stmt, s => 1);
                let stmts = self.parse(&stmt.children[0])?.stmts;
                Stmt::Warp { stmts, comment }
            }
            "doDeleteFromList" => {
                let comment = check_children_get_comment!(self, stmt, s => 2);
                let list = self.parse_expr(&stmt.children[1])?;
                match stmt.children[0].get(&["option"]) {
                    Some(opt) => match opt.text.as_str() {
                        "last" => Stmt::Pop { list, comment },
                        "all" => Stmt::RemoveAll { list, comment },
                        "" => return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }),
                        x => return Err(Error::InvalidProject { error: ProjectError::BlockOptionUnknown { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into(), got: x.into() } }),
                    }
                    None => {
                        let index = self.parse_expr(&stmt.children[0])?;
                        Stmt::RemoveAt { list, index, comment }
                    }
                }
            }
            "doInsertInList" => {
                let comment = check_children_get_comment!(self, stmt, s => 3);
                let value = self.parse_expr(&stmt.children[0])?;
                let list = self.parse_expr(&stmt.children[2])?;
                match stmt.children[1].get(&["option"]) {
                    Some(opt) => match opt.text.as_str() {
                        "last" => Stmt::Push { list, value, comment },
                        "random" | "any" => Stmt::InsertAtRand { list, value, comment },
                        "" => return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }),
                        x => return Err(Error::InvalidProject { error: ProjectError::BlockOptionUnknown { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into(), got: x.into() } }),
                    }
                    None => {
                        let index = self.parse_expr(&stmt.children[0])?;
                        Stmt::InsertAt { list, value, index, comment }
                    }
                }
            }
            "doReplaceInList" => {
                let comment = check_children_get_comment!(self, stmt, s => 3);
                let value = self.parse_expr(&stmt.children[2])?;
                let list = self.parse_expr(&stmt.children[1])?;
                match stmt.children[0].get(&["option"]) {
                    Some(opt) => match opt.text.as_str() {
                        "last" => Stmt::LastIndexAssign { list, value, comment },
                        "random" | "any" => Stmt::RandIndexAssign { list, value, comment },
                        "" => return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }),
                        x => return Err(Error::InvalidProject { error: ProjectError::BlockOptionUnknown { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into(), got: x.into() } }),
                    }
                    None => {
                        let index = self.parse_expr(&stmt.children[0])?;
                        Stmt::IndexAssign { list, value, index, comment }
                    }
                }
            }
            "doAddToList" => binary_op!(self, stmt, s => Stmt::Push : value, list),
            "doReport" => unary_op!(self, stmt, s => Stmt::Return : value),
            "doWait" => unary_op!(self, stmt, s => Stmt::Sleep : seconds),
            _ => return Err(Error::UnknownBlockType { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.to_owned() }),
        })
    }
    fn reference_var(&self, name: String) -> Result<VariableRef, Error> {
        macro_rules! check_locations {
            ($($sym:expr => $loc:expr),*$(,)?) => {$({
                if let Some(def) = $sym.get(&name) { return Ok(def.ref_at($loc)) }
            })*}
        }
        check_locations!(&self.locals => VarLocation::Local, &self.sprite.fields => VarLocation::Field, &self.role.globals => VarLocation::Global);
        Err(Error::UndefinedVariable { role: self.role.name.clone(), sprite: self.sprite.name.clone(), name })
    }
    fn parse_expr(&self, expr: &Xml) -> Result<Expr, Error> {
        match expr.name.as_str() {
            "l" => match expr.text.parse::<f64>() {
                Ok(v) => Ok(v.into()),
                Err(_) => Ok(expr.text.clone().into()),
            }
            "bool" => match expr.text.as_str() {
                "true" => Ok(true.into()),
                "false" => Ok(false.into()),
                x => return Err(Error::InvalidProject { error: ProjectError::BoolUnknownValue { role: self.role.name.clone(), sprite: self.sprite.name.clone(), value: x.into() } })
            }
            "list" => match expr.attr("struct") {
                Some(v) if v.value == "atomic" => match serde_json::from_str::<JsonValue>(&format!("[{}]", expr.text)) {
                    Err(_) => return Err(Error::InvalidProject { error: ProjectError::InvalidJson { reason: format!("content was not json: [{}]", expr.text) } }),
                    Ok(json) => Ok(Value::try_from(json)?.into()),
                }
                _ => {
                    let mut values = Vec::with_capacity(expr.children.len());
                    for item in expr.children.iter() {
                        match item.children.get(0) {
                            None => return Err(Error::InvalidProject { error: ProjectError::ListItemNoValue { role: self.role.name.clone(), sprite: self.sprite.name.clone() } }),
                            Some(x) => match self.parse_expr(x)? {
                                Expr::Value(v) => values.push(v),
                                _ => return Err(Error::InvalidProject { error: ProjectError::ValueNotEvaluated { role: self.role.name.clone(), sprite: Some(self.sprite.name.clone()) } }),
                            }
                        }
                    }
                    Ok(values.into())
                }
            }
            "block" => {
                if let Some(var) = expr.attr("var") {
                    let comment = check_children_get_comment!(self, expr, "var" => 0);
                    let var = self.reference_var(var.value.clone())?;
                    return Ok(Expr::Variable { var, comment });
                }
                let s = match expr.attr("s") {
                    None => return Err(Error::InvalidProject { error: ProjectError::BlockWithoutType { role: self.role.name.clone(), sprite: self.sprite.name.clone() } }),
                    Some(v) => v.value.as_str(),
                };
                Ok(match s {
                    "reportSum" => binary_op!(self, expr, s => Expr::Add),
                    "reportDifference" => binary_op!(self, expr, s => Expr::Sub),
                    "reportProduct" => binary_op!(self, expr, s => Expr::Mul),
                    "reportQuotient" => binary_op!(self, expr, s => Expr::Div),
                    "reportModulus" => binary_op!(self, expr, s => Expr::Mod),
                    "reportPower" => binary_op!(self, expr, s => Expr::Pow : base, power),

                    "reportAnd" => binary_op!(self, expr, s => Expr::And),
                    "reportOr" => binary_op!(self, expr, s => Expr::Or),

                    "reportIsIdentical" => binary_op!(self, expr, s => Expr::RefEq),
                    "reportEquals" => binary_op!(self, expr, s => Expr::Eq),
                    "reportLessThan" => binary_op!(self, expr, s => Expr::Less),
                    "reportGreaterThan" => binary_op!(self, expr, s => Expr::Greater),

                    "reportRandom" => binary_op!(self, expr, s => Expr::RandInclusive : a, b),
                    "reportNumbers" => binary_op!(self, expr, s => Expr::RangeInclusive : start, stop),

                    "reportNot" => unary_op!(self, expr, s => Expr::Not),
                    "reportRound" => unary_op!(self, expr, s => Expr::Round),

                    "reportListLength" => unary_op!(self, expr, s => Expr::Listlen),
                    "reportListIsEmpty" => {
                        let comment = check_children_get_comment!(self, expr, s => 1);
                        let value = self.parse_expr(&expr.children[0])?.into();
                        Expr::Greater { left: Box::new(Expr::Listlen { value, comment: None }), right: Box::new(0.0f64.into()), comment }
                    }

                    "reportListIndex" => binary_op!(self, expr, s => Expr::ListFind : value, list),
                    "reportListContainsItem" => {
                        let comment = check_children_get_comment!(self, expr, s => 2);
                        let value = self.parse_expr(&expr.children[0])?.into();
                        let list = self.parse_expr(&expr.children[1])?.into();
                        Expr::Greater { left: Box::new(Expr::ListFind { value, list, comment: None }), right: Box::new(0.0f64.into()), comment }
                    }
                    "reportListItem" => {
                        let comment = check_children_get_comment!(self, expr, s => 2);
                        let list = self.parse_expr(&expr.children[1])?.into();
                        match expr.children[0].get(&["option"]) {
                            Some(opt) => match opt.text.as_str() {
                                "last" => Expr::ListLastIndex { list, comment },
                                "any" => Expr::ListRandIndex { list, comment },
                                "" => return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }),
                                x => return Err(Error::InvalidProject { error: ProjectError::BlockOptionUnknown { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into(), got: x.into() } }),
                            }
                            None => {
                                let index = self.parse_expr(&expr.children[0])?.into();
                                Expr::ListIndex { list, index, comment }
                            }
                        }
                    }

                    "reportStringSize" => unary_op!(self, expr, s => Expr::Strlen),
                    "reportUnicodeAsLetter" => unary_op!(self, expr, s => Expr::UnicodeToChar),
                    "reportUnicode" => unary_op!(self, expr, s => Expr::CharToUnicode),

                    "reportCDR" => unary_op!(self, expr, s => Expr::ListSlice { from: Some(Box::new(2.0f64.into())), to: None }),
                    "reportCONS" => {
                        let comment = check_children_get_comment!(self, expr, s => 2);
                        let val = self.parse_expr(&expr.children[0])?;
                        let list = self.parse_expr(&expr.children[0])?;
                        Expr::Listcat { lists: vec![val, list], comment}
                    }

                    "reportJoinWords" => variadic_op!(self, expr, s => Expr::Strcat),
                    "reportConcatenatedLists" => variadic_op!(self, expr, s => Expr::Listcat : lists),
                    "reportNewList" => variadic_op!(self, expr, s => Expr::MakeList),

                    "reportBoolean" => match expr.get(&["l", "bool"]) {
                        Some(v) if v.text == "true" => true.into(),
                        Some(v) if v.text == "false" => false.into(),
                        _ => return Err(Error::InvalidProject { error: ProjectError::InvalidBoolLiteral { role: self.role.name.clone(), sprite: self.sprite.name.clone() } }),
                    }
                    "reportMonadic" => {
                        let comment = check_children_get_comment!(self, expr, s => 2);
                        let func = match expr.children[0].get(&["option"]) {
                            None => return Err(Error::InvalidProject { error: ProjectError::BlockMissingOption { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() } }),
                            Some(f) => {
                                if f.children.len() != 0 { return Err(Error::BlockOptionNotConst { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }) }
                                f.text.as_str()
                            }
                        };
                        let value = Box::new(self.parse_expr(&expr.children[1])?);
                        match func {
                            "id" => *value,

                            "neg" => Expr::Neg { value, comment },
                            "abs" => Expr::Abs { value, comment },
                            "sqrt" => Expr::Sqrt { value, comment },
                            "floor" => Expr::Floor { value, comment },
                            "ceiling" => Expr::Ceil { value, comment },

                            "sin" => Expr::Sin { value, comment },
                            "cos" => Expr::Cos { value, comment },
                            "tan" => Expr::Tan { value, comment },

                            "asin" => Expr::Asin { value, comment },
                            "acos" => Expr::Acos { value, comment },
                            "atan" => Expr::Atan { value, comment },

                            "ln" => Expr::Log { value, base: Box::new(Constant::E.into()), comment },
                            "lg" => Expr::Log { value, base: Box::new(2f64.into()), comment },
                            "log" => Expr::Log { value, base: Box::new(10f64.into()), comment },

                            "e^" => Expr::Pow { base: Box::new(Constant::E.into()), power: value, comment },
                            "2^" => Expr::Pow { base: Box::new(2f64.into()), power: value, comment },
                            "10^" => Expr::Pow { base: Box::new(10f64.into()), power: value, comment },

                            "" => return Err(Error::BlockOptionNotSelected { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into() }),
                            _ => return Err(Error::InvalidProject { error: ProjectError::BlockOptionUnknown { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.into(), got: func.into() } }),
                        }
                    }
                    "reportIfElse" => {
                        let comment = check_children_get_comment!(self, expr, s => 3);
                        let condition = Box::new(self.parse_expr(&expr.children[0])?);
                        let then = Box::new(self.parse_expr(&expr.children[1])?);
                        let otherwise = Box::new(self.parse_expr(&expr.children[2])?);
                        Expr::Conditional { condition, then, otherwise, comment }
                    }
                    _ => return Err(Error::UnknownBlockType { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: s.to_owned() }),
                })
            }
            x => return Err(Error::UnknownBlockType { role: self.role.name.clone(), sprite: self.sprite.name.clone(), block_type: x.into() }),
        }
    }
}

struct SpriteInfo<'a> {
    parser: &'a Parser,
    role: &'a RoleInfo<'a>,
    name: String,
    fields: SymbolTable<'a>,
}
impl<'a> SpriteInfo<'a> {
    fn new(role: &'a RoleInfo, name: String) -> Self {
        Self { parser: role.parser, role, name, fields: SymbolTable::new(role.parser) }
    }
    fn parse(mut self, sprite: &Xml) -> Result<Sprite, Error> {
        if let Some(fields) = sprite.get(&["variables"]) {
            let dummy_script = ScriptInfo::new(&self);

            let mut defs = vec![];
            for def in fields.children.iter().filter(|v| v.name == "variable") {
                let name = match def.attr("name") {
                    None => return Err(Error::InvalidProject { error: ProjectError::UnnamedField { role: self.role.name.clone(), sprite: self.name } }),
                    Some(x) => x.value.clone(),
                };
                let value = match def.children.get(0) {
                    None => return Err(Error::InvalidProject { error: ProjectError::FieldNoValue { role: self.role.name.clone(), sprite: self.name, name } }),
                    Some(x) => match dummy_script.parse_expr(x)? {
                        Expr::Value(v) => v,
                        _ => return Err(Error::InvalidProject { error: ProjectError::ValueNotEvaluated { role: self.role.name.clone(), sprite: Some(self.name) } }),
                    }
                };
                defs.push((name, value));
            }

            for (name, value) in defs {
                match self.fields.define(name.clone(), value) {
                    Ok(None) => (),
                    Ok(Some(prev)) => return Err(Error::InvalidProject { error: ProjectError::FieldsWithSameName { role: self.role.name.clone(), sprite: self.name.clone(), name: prev.name } }),
                    Err(SymbolError::NameTransformError { name }) => return Err(Error::NameTransformError { name, role: Some(self.role.name.clone()), sprite: Some(self.name.clone()) }),
                    Err(SymbolError::ConflictingTrans { trans_name, names }) => return Err(Error::FieldsWithSameTransName { role: self.role.name.clone(), sprite: self.name.clone(), trans_name, names }),
                }
            }
        }

        let mut scripts = vec![];
        if let Some(scripts_xml) = sprite.get(&["scripts"]) {
            for script_xml in scripts_xml.children.iter() {
                match script_xml.children.as_slice() {
                    [] => continue,
                    [stmt] => {
                        if stmt.attr("var").is_some() { continue }
                        if let Some(s) = stmt.attr("s") {
                            if s.value.starts_with("report") { continue }
                        }
                    }
                    _ => (),
                }
                scripts.push(ScriptInfo::new(&self).parse(script_xml)?);
            }
        }

        Ok(Sprite { name: self.name, fields: self.fields.into_defs(), scripts })
    }
}

struct RoleInfo<'a> {
    parser: &'a Parser,
    name: String,
    globals: SymbolTable<'a>,
}
impl<'a> RoleInfo<'a> {
    fn new(parser: &'a Parser, name: String) -> Self {
        Self { parser, name, globals: SymbolTable::new(parser) }
    }
    fn parse(mut self, role_root: &Xml) -> Result<Role, Error> {
        assert_eq!(role_root.name, "role");
        let role = match role_root.attr("name") {
            None => return Err(Error::InvalidProject { error: ProjectError::UnnamedRole }),
            Some(x) => x.value.clone(),
        };
        let content = match role_root.get(&["project"]) {
            None => return Err(Error::InvalidProject { error: ProjectError::NoRoleContent { role } }),
            Some(x) => x,
        };
        let notes = content.get(&["notes"]).map(|v| v.text.as_str()).unwrap_or("").to_owned();
        let stage = match content.get(&["stage"]) {
            None => return Err(Error::InvalidProject { error: ProjectError::NoStageDef { role } }),
            Some(x) => x,
        };

        if let Some(globals) = content.get(&["variables"]) {
            let dummy_sprite = SpriteInfo::new(&self, "global".into());
            let dummy_script = ScriptInfo::new(&dummy_sprite);

            let mut defs = vec![];
            for def in globals.children.iter().filter(|v| v.name == "variable") {
                let name = match def.attr("name") {
                    None => return Err(Error::InvalidProject { error: ProjectError::UnnamedGlobal { role } }),
                    Some(x) => x.value.clone(),
                };
                let value = match def.children.get(0) {
                    None => return Err(Error::InvalidProject { error: ProjectError::GlobalNoValue { role, name } }),
                    Some(x) => match dummy_script.parse_expr(x)? {
                        Expr::Value(v) => v,
                        _ => return Err(Error::InvalidProject { error: ProjectError::ValueNotEvaluated { role, sprite: None } }),
                    }
                };
                defs.push((name, value));
            }

            for (name, value) in defs {
                match self.globals.define(name.clone(), value) {
                    Ok(None) => (),
                    Ok(Some(prev)) => return Err(Error::InvalidProject { error: ProjectError::GlobalsWithSameName { role: self.name.clone(), name: prev.name } }),
                    Err(SymbolError::NameTransformError { name }) => return Err(Error::NameTransformError { name, role: Some(self.name.clone()), sprite: None }),
                    Err(SymbolError::ConflictingTrans { trans_name, names }) => return Err(Error::GlobalsWithSameTransName { role: self.name.clone(), trans_name, names }),
                }
            }
        }

        let mut sprites = vec![];
        if let Some(sprites_xml) = stage.get(&["sprites"]) {
            for sprite in iter::once(stage).chain(sprites_xml.children.iter().filter(|s| s.name == "sprite")) {
                let name = match sprite.attr("name") {
                    None => return Err(Error::InvalidProject { error: ProjectError::UnnamedSprite { role } }),
                    Some(x) => x.value.clone(),
                };
                sprites.push(SpriteInfo::new(&self, name).parse(sprite)?);
            }
        }

        Ok(Role { name: role, notes, globals: self.globals.into_defs(), sprites })
    }
}

#[derive(Builder)]
pub struct Parser {
    #[builder(default = "false")]
    optimize: bool,
    #[builder(default = "Rc::new(|v| Ok(v.into()))")]
    name_transformer: Rc<dyn Fn(&str) -> Result<String, ()>>,
}
impl Parser {
    fn opt(&self, project: Project) -> Result<Project, Error> {
        Ok(project)
    }
    pub fn parse<R: Read>(&self, xml: R) -> Result<Project, Error> {
        let mut xml = EventReader::new(xml);
        while let Ok(e) = xml.next() {
            if let XmlEvent::StartElement { name, attributes, .. } = e {
                if name.local_name != "room" { continue }
                let project = parse_xml_root(&mut xml, name, attributes)?;
                let proj_name = project.attr("name").map(|v| v.value.as_str()).unwrap_or("untitled").to_owned();

                let mut roles = Vec::with_capacity(project.children.len());
                for child in project.children.iter() {
                    if child.name == "role" {
                        let role_name = match child.attr("name") {
                            None => return Err(Error::InvalidProject { error: ProjectError::UnnamedRole }),
                            Some(x) => x.value.clone(),
                        };
                        roles.push(RoleInfo::new(self, role_name).parse(child)?);
                    }
                }

                let mut project = Some(Project { name: proj_name, roles });
                if self.optimize { project = Some(self.opt(mem::take(&mut project).unwrap())?) }
                return Ok(project.unwrap())
            }
        }
        Err(Error::InvalidProject { error: ProjectError::NoRoot })
    }
}
