// JSON Schema validation (drafts 2020-12 and 7) without eval: compile() builds a schema into plain checks once.
// The builders return plain JSON Schema objects.

pub mod builders;
pub mod checks;
pub mod coerce;
pub mod compiler;
pub mod explain;
pub mod formats;
pub mod regex;
pub mod resolver;
pub mod types;
pub mod validator;
pub mod value;

pub use builders::{
    Annotations, ArrOptions, BigintBounds, BigintOptions, LengthBounds, NumOptions, Optional,
    Properties, StrOptions, arr, bigint, bool, bytes, int, lit, map, nul, nullable, num, obj,
    one_of, opt, recursive, str, tuple, union, unknown, unsafe_schema,
};
pub use types::{
    CompileError, CompileOptions, Draft, FormatCheck, JsonSchema, ParseResult, UnresolvedRefs,
    ValidationError,
};
pub use validator::{CoercingValidator, Validator, compile, compile_coercing, compile_with};
pub use value::Value;
pub use value::equal;
