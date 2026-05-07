/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod emit;
pub mod extract;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dupe::Dupe;
    use pyrefly_util::forgetter::Forgetter;
    use pyrefly_util::fs_anyhow;
    use pyrefly_util::globs::FilteredGlobs;
    use pyrefly_util::globs::Globs;
    use pyrefly_util::globs::HiddenDirFilter;
    use pyrefly_util::includes::Includes;
    use pyrefly_util::thread_pool::TEST_THREAD_COUNT;

    use super::emit::emit_stub;
    use super::extract::ExtractConfig;
    use super::extract::extract_module_stub;
    use crate::state::require::Require;
    use crate::state::state::State;
    use crate::test::util::TestEnv;

    fn run_stubgen(input: &str) -> String {
        run_stubgen_with_config(
            input,
            &ExtractConfig {
                include_private: false,
                include_docstrings: false,
            },
        )
    }

    fn run_stubgen_with_config(input: &str, config: &ExtractConfig) -> String {
        // Minimal `pydantic` definitions for stub extraction tests. The real package applies
        // `@dataclass_transform(kw_only_default=True)`, which makes every field keyword-only and
        // does not match the constructor shapes these tests assert against.
        const PYDANTIC_SHIM: &str = r#"from typing import dataclass_transform

def Field(**kwargs): ...

def computed_field(fn=None, **_kw):
    return fn

@dataclass_transform(
    kw_only_default=False,
    field_specifiers=(Field,),
)
class BaseModel:
    pass
"#;

        let tdir = tempfile::tempdir().unwrap();
        let pydantic_pkg = tdir.path().join("pydantic");
        fs_anyhow::create_dir_all(&pydantic_pkg).unwrap();
        fs_anyhow::write(&pydantic_pkg.join("__init__.py"), PYDANTIC_SHIM).unwrap();

        let path = tdir.path().join("input.py");
        fs_anyhow::write(&path, input).unwrap();
        let mut t = TestEnv::new();
        // Real files under `tdir` are picked up by the stubgen glob; registering `pydantic` here
        // wires the package into `ConfigFinder`'s source DB so imports resolve during checking.
        t.add_real_path("pydantic", pydantic_pkg.join("__init__.py"));
        t.add(&path.display().to_string(), input);
        // Limit checking to the snippet under test. A sibling `pydantic/` package may live in the
        // same temp directory for imports; it must not be treated as another stubgen input file.
        let includes = Globs::new(vec![format!("{}/input.py", tdir.path().display())]).unwrap();
        let f_globs = Box::new(FilteredGlobs::new(
            includes,
            Globs::empty(),
            None,
            HiddenDirFilter::Disabled,
        ));
        let config_finder = t.config_finder();

        let expanded = config_finder.checkpoint(f_globs.files()).unwrap();
        let state = State::new(config_finder, TEST_THREAD_COUNT);
        let holder = Forgetter::new(state, false);

        let handles_obj = crate::commands::check::Handles::new(expanded);
        let mut forgetter = Forgetter::new(
            holder.as_ref().new_transaction(Require::Everything, None),
            true,
        );
        let transaction = forgetter.as_mut();

        let (handles, _, _) = handles_obj.all(holder.as_ref().config_finder());

        let mut result = String::new();
        for handle in &handles {
            transaction.run(&[handle.dupe()], Require::Everything, None);
            if let Some(stub) = extract_module_stub(transaction, handle, config) {
                result = emit_stub(&stub);
            }
        }
        result
    }

    /// Get the path to the stubgen test fixtures directory.
    fn get_test_dir() -> PathBuf {
        let test_path = std::env::var("STUBGEN_TEST_PATH")
            .expect("STUBGEN_TEST_PATH env var not set: buck should set this automatically");
        let mut dir = std::env::current_dir().expect("Failed to get current directory");
        dir.push(test_path);
        dir
    }

    /// Run a snapshot test for a specific test case directory.
    fn assert_stubgen_snapshot(test_name: &str) {
        let test_dir = get_test_dir().join(test_name);
        let input_path = test_dir.join("input.py");
        let expected_path = test_dir.join("expected.pyi");

        assert!(
            input_path.exists(),
            "Input file does not exist: {}",
            input_path.display()
        );

        let input = fs_anyhow::read_to_string(&input_path).unwrap();
        let actual = run_stubgen(&input);

        if std::env::var("STUBGEN_UPDATE_SNAPSHOTS").is_ok() {
            fs_anyhow::create_dir_all(&test_dir).unwrap();
            let out = test_dir.join("expected.pyi");
            fs_anyhow::write(&out, &actual).unwrap();
            println!("Updated snapshot for {} -> {}", test_name, out.display());
            return;
        }

        assert!(
            expected_path.exists(),
            "Expected file does not exist: {}\nRun with STUBGEN_UPDATE_SNAPSHOTS=1 to generate.",
            expected_path.display()
        );

        let expected = fs_anyhow::read_to_string(&expected_path)
            .unwrap()
            .replace("\r\n", "\n");
        // Strip the AT generated header and trim whitespace so the expected
        // files can keep the header for tooling without affecting comparison.
        let expected = expected
            .strip_prefix(&format!("# @{}generated\n", "")) // Avoid this file from being recognized as generated
            .unwrap_or(&expected)
            .trim()
            .to_owned();
        let actual = actual.replace("\r\n", "\n").trim().to_owned();

        pretty_assertions::assert_str_eq!(
            expected,
            actual,
            "Stub mismatch for {test_name}.\nTo update, run with STUBGEN_UPDATE_SNAPSHOTS=1."
        );
    }

    #[test]
    fn test_stubgen_functions() {
        assert_stubgen_snapshot("functions");
    }

    #[test]
    fn test_stubgen_classes() {
        assert_stubgen_snapshot("classes");
    }

    #[test]
    fn test_stubgen_variables() {
        assert_stubgen_snapshot("variables");
    }

    #[test]
    fn test_stubgen_imports() {
        assert_stubgen_snapshot("imports");
    }

    #[test]
    fn test_stubgen_mixed() {
        assert_stubgen_snapshot("mixed");
    }

    #[test]
    fn test_stubgen_overloads() {
        assert_stubgen_snapshot("overloads");
    }

    #[test]
    fn test_stubgen_typevar() {
        assert_stubgen_snapshot("typevar");
    }

    #[test]
    fn test_stubgen_type_alias_old_style() {
        assert_stubgen_snapshot("type_alias_old_style");
    }

    #[test]
    fn test_stubgen_generics() {
        assert_stubgen_snapshot("generics");
    }

    #[test]
    fn test_stubgen_dunder_all() {
        assert_stubgen_snapshot("dunder_all");
    }

    #[test]
    fn test_stubgen_docstrings() {
        let input = r#"
def greet(name: str) -> str:
    """Say hello."""
    return f"Hello, {name}!"

def no_doc(x: int) -> int:
    return x

class MyClass:
    """A class with a docstring."""

    def method(self) -> None:
        """Do something."""
        pass
"#;
        let config = ExtractConfig {
            include_private: false,
            include_docstrings: true,
        };
        let actual = run_stubgen_with_config(input, &config);
        assert!(
            actual.contains(r#""""Say hello.""""#),
            "Function docstring should be emitted:\n{actual}"
        );
        assert!(
            actual.contains(r#""""A class with a docstring.""""#),
            "Class docstring should be emitted:\n{actual}"
        );
        assert!(
            actual.contains(r#""""Do something.""""#),
            "Method docstring should be emitted:\n{actual}"
        );

        // Without docstrings, none should appear.
        let no_doc_config = ExtractConfig {
            include_private: false,
            include_docstrings: false,
        };
        let without = run_stubgen_with_config(input, &no_doc_config);
        assert!(
            !without.contains("Say hello."),
            "Function docstring should not appear with include_docstrings=false:\n{without}"
        );
        assert!(
            !without.contains("A class with a docstring."),
            "Class docstring should not appear with include_docstrings=false:\n{without}"
        );
    }

    #[test]
    fn test_stubgen_unannotated_dunder_new_uses_self() {
        let actual = run_stubgen(
            r#"
class C:
    def __new__(cls):
        return super().__new__(cls)
"#,
        );
        pretty_assertions::assert_str_eq!(
            r#"
from typing import Self

class C:
    def __new__(cls) -> Self: ...
"#
            .trim(),
            actual.trim(),
        );
    }

    /// Dataclass `field(...)` defaults are elided as `= ...` on the attribute; typing-relevant init
    /// shape is reflected in an explicit `__init__` (here: optional with default).
    #[test]
    fn test_stubgen_dataclass_field_strips_metadata() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class A:
    n: int = field(default=1, metadata={"a": 1}, repr=True)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class A:
    n: int = ...

    def __init__(self, n: int = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Non-literal `field(default=None)` becomes `= ...` on the stub attribute; `__init__` carries
    /// the optional parameter default (fixes #3221-style missing-argument errors against stubs).
    #[test]
    fn test_stubgen_dataclass_field_strips_field_call() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class A:
    name: str | None = field(default=None)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class A:
    name: str | None = ...

    def __init__(self, name: str | None = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `default_factory` is elided as `= ...` on the attribute; the synthesized `__init__` keeps a
    /// default placeholder for the factory-produced value.
    #[test]
    fn test_stubgen_dataclass_field_retains_default_factory() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class A:
    items: list[str] = field(default_factory=list, metadata={"k": 1})
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class A:
    items: list[str] = ...

    def __init__(self, items: list[str] = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Same elision + explicit `__init__` for Pydantic models (`Field(...)` not re-emitted on the
    /// class body when the default is non-literal / factory-driven).
    #[test]
    fn test_stubgen_pydantic_field_retains_default_factory() {
        let input = r#"from pydantic import BaseModel, Field

class A(BaseModel):
    items: list[str] = Field(default_factory=list, min_length=0)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class A(BaseModel):
    items: list[str] = ...

    def __init__(self, items: list[str] = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Complex `default_factory` still becomes `= ...` on the attribute; init default is elided the
    /// same way.
    #[test]
    fn test_stubgen_dataclass_field_default_factory_complex_uses_placeholder() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class A:
    x: list[int] = field(default_factory=lambda: [1, 2, 3])
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class A:
    x: list[int] = ...

    def __init__(self, x: list[int] = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    #[test]
    fn test_stubgen_pydantic_field_default_factory_complex_uses_placeholder() {
        let input = r#"import functools
from pydantic import BaseModel, Field

class A(BaseModel):
    x: list[int] = Field(default_factory=functools.partial(list, [0]))
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class A(BaseModel):
    x: list[int] = ...

    def __init__(self, x: list[int] = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    #[test]
    fn test_stubgen_pydantic_field_strips_field_call() {
        let input = r#"from pydantic import BaseModel, Field

class A(BaseModel):
    x: int = Field(default=0)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class A(BaseModel):
    x: int = ...

    def __init__(self, x: int = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    #[test]
    fn test_stubgen_pydantic_field_required_strips_description() {
        let input = r#"from pydantic import BaseModel, Field

class A(BaseModel):
    u: int = Field(..., description="required")
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class A(BaseModel):
    u: int

    def __init__(self, u: int) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `include_docstrings` must not add inline `# ...` Field metadata comments on attributes; stub
    /// shape stays the same as without doc config for this case (no `Field(...)` on the class body).
    #[test]
    fn test_stubgen_pydantic_field_retains_description_with_include_docstrings() {
        let input = r#"from pydantic import BaseModel, Field

class A(BaseModel):
    u: int = Field(..., description="required", title="T")
"#;
        let config = ExtractConfig {
            include_private: false,
            include_docstrings: true,
        };
        let actual = run_stubgen_with_config(input, &config);
        let expected = r#"from pydantic import BaseModel


class A(BaseModel):
    u: int

    def __init__(self, u: int) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// A bare `field()` (no default / factory) is a required init parameter; class attribute has no
    /// default RHS.
    #[test]
    fn test_stubgen_dataclass_field_bare_preserved() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class A:
    name: str = field()
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class A:
    name: str

    def __init__(self, name: str) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Mix of required fields, literals, `field()` defaults, and `kw_only`; `__init__` uses `*` so
    /// keyword-only fields match dataclass semantics.
    #[test]
    fn test_stubgen_dataclass_mixed_field_defaults() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class Mixed:
    required: int
    literal_default: str = "x"
    field_with_default: int = field(default=0)
    field_with_none: str | None = field(default=None)
    field_no_default: int = field()
    field_ellipsis: int = field(..., kw_only=True)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class Mixed:
    required: int
    literal_default: str = "x"
    field_with_default: int = ...
    field_with_none: str | None = ...
    field_no_default: int
    field_ellipsis: int

    def __init__(
        self,
        required: int,
        literal_default: str = "x",
        field_with_default: int = ...,
        field_with_none: str | None = ...,
        field_no_default: int,
        *,
        field_ellipsis: int,
    ) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Pydantic `Field(...)` mixed cases: same pattern—elided non-literal defaults as `= ...`, bare
    /// / required fields without RHS defaults, explicit `__init__` for checker-visible semantics.
    #[test]
    fn test_stubgen_pydantic_mixed_field_defaults() {
        let input = r#"from pydantic import BaseModel, Field

class Mixed(BaseModel):
    required: int
    literal_default: int = 7
    field_with_default: int = Field(default=0)
    field_with_none: str | None = Field(default=None)
    field_bare: int = Field()
    field_ellipsis: int = Field(..., description="req")
    field_ellipsis_only: int = Field(...)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class Mixed(BaseModel):
    required: int
    literal_default: int = 7
    field_with_default: int = ...
    field_with_none: str | None = ...
    field_bare: int
    field_ellipsis: int
    field_ellipsis_only: int

    def __init__(
        self,
        required: int,
        literal_default: int = 7,
        field_with_default: int = ...,
        field_with_none: str | None = ...,
        field_bare: int,
        field_ellipsis: int,
        field_ellipsis_only: int,
    ) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `Field(init=False)` keeps the attribute on the class (with elided default) but drops it from
    /// the synthesized `__init__`, matching runtime construction.
    #[test]
    fn test_stubgen_pydantic_field_init_false() {
        let input = r#"from pydantic import BaseModel, Field

class Model(BaseModel):
    id: int
    digest: bytes = Field(default=b"", init=False)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class Model(BaseModel):
    id: int
    digest: bytes = ...

    def __init__(self, id: int) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Validation aliases determine acceptable constructor keywords at runtime; the stub exposes
    /// that surface using the alias name as a keyword-only parameter (field name stays the stored
    /// attribute).
    #[test]
    fn test_stubgen_pydantic_field_validation_alias_on_init() {
        let input = r#"from pydantic import BaseModel, Field

class Row(BaseModel):
    internal_id: int = Field(validation_alias="id")
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class Row(BaseModel):
    internal_id: int

    def __init__(self, *, id: int) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `Field(kw_only=True)` marks trailing constructor parameters as keyword-only (`*` before
    /// those params), analogous to dataclass `kw_only`.
    #[test]
    fn test_stubgen_pydantic_field_kw_only() {
        let input = r#"from pydantic import BaseModel, Field

class Args(BaseModel):
    req: int = Field(kw_only=False)
    opt: str = Field(default="z", kw_only=True)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class Args(BaseModel):
    req: int
    opt: str = ...

    def __init__(self, req: int, *, opt: str = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `@computed_field` contributes a read-only attribute without an `__init__` parameter; emit it
    /// as a normal `@property` stub with an elided body.
    #[test]
    fn test_stubgen_pydantic_computed_field_as_property() {
        let input = r#"from pydantic import BaseModel, computed_field

class Rect(BaseModel):
    width: int
    height: int

    @computed_field
    @property
    def area(self) -> int:
        return self.width * self.height
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class Rect(BaseModel):
    width: int
    height: int

    def __init__(self, width: int, height: int) -> None: ...

    @property
    def area(self) -> int: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `init=False` fields are still typed on the class (with elided defaults when appropriate) but
    /// must not appear as parameters on the synthesized `__init__`.
    #[test]
    fn test_stubgen_dataclass_field_init_false() {
        let input = r#"from dataclasses import dataclass, field

@dataclass
class Config:
    path: str
    cached: dict[str, str] = field(default_factory=dict, init=False)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class Config:
    path: str
    cached: dict[str, str] = ...

    def __init__(self, path: str) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `InitVar` parameters participate in `__init__` with their wrapped type; the `InitVar[...]`
    /// annotation stays on the class body and does not represent a stored instance attribute.
    #[test]
    fn test_stubgen_dataclass_initvar() {
        let input = r#"from dataclasses import dataclass, InitVar, field

@dataclass
class Wrapped:
    raw: InitVar[bytes | None]
    text: str = field(default="")
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass, InitVar


@dataclass
class Wrapped:
    raw: InitVar[bytes | None]
    text: str = ...

    def __init__(self, raw: bytes | None, text: str = ...) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `ClassVar` annotations mark class-level state: they stay on the class body but are omitted
    /// from the synthesized instance `__init__`.
    #[test]
    fn test_stubgen_dataclass_classvar() {
        let input = r#"from dataclasses import dataclass
from typing import ClassVar

@dataclass
class A:
    sentinel: ClassVar[str] = "x"
    value: int
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass
from typing import ClassVar


@dataclass
class A:
    sentinel: ClassVar[str] = "x"
    value: int

    def __init__(self, value: int) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// `@dataclass(kw_only=True)` makes every dataclass field keyword-only in `__init__`; the stub
    /// reflects that with `*` before all constructor parameters.
    #[test]
    fn test_stubgen_dataclass_kw_only_class_flag() {
        let input = r#"from dataclasses import dataclass

@dataclass(kw_only=True)
class B:
    a: int
    b: str = "y"
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass(kw_only=True)
class B:
    a: int
    b: str = "y"

    def __init__(self, *, a: int, b: str = "y") -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// An explicit user-defined `__init__` must be kept as the sole constructor stub; stubgen must
    /// not replace it with a synthesized dataclass signature derived from fields.
    #[test]
    fn test_stubgen_dataclass_preserves_explicit_init() {
        let input = r#"from dataclasses import dataclass

@dataclass
class CustomInit:
    x: int

    def __init__(self, x: int, *, tag: str = "") -> None:
        self.x = x
"#;
        let actual = run_stubgen(input);
        let expected = r#"from dataclasses import dataclass


@dataclass
class CustomInit:
    x: int

    def __init__(self, x: int, *, tag: str = "") -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    /// Same for Pydantic: a handwritten `__init__` overrides constructor synthesis for that class.
    #[test]
    fn test_stubgen_pydantic_preserves_explicit_init() {
        let input = r#"from pydantic import BaseModel

class CustomInit(BaseModel):
    name: str

    def __init__(self, name: str, *, eager: bool = False) -> None:
        super().__init__(name=name)
"#;
        let actual = run_stubgen(input);
        let expected = r#"from pydantic import BaseModel


class CustomInit(BaseModel):
    name: str

    def __init__(self, name: str, *, eager: bool = False) -> None: ...
"#;
        pretty_assertions::assert_str_eq!(expected, &actual);
    }

    #[test]
    fn test_stubgen_dataclass_repr_false_repr_rebind_uses_classvar() {
        let actual = run_stubgen(
            r#"
from dataclasses import dataclass

def _repr_fn(self: object) -> str:
    return "x"

@dataclass(repr=False)
class C:
    __repr__ = _repr_fn
"#,
        );
        pretty_assertions::assert_str_eq!(
            r#"
from typing import Callable, ClassVar

from dataclasses import dataclass


@dataclass(repr=False)
class C:
    __repr__: ClassVar[Callable[[object], str]] = ...
"#
            .trim(),
            actual.trim(),
        );
    }

    /// Class-body assignment to an `@overload` group must print as `Overload[Callable[...], ...]`
    /// (annotation-safe) and pull `Overload` into the stub typing import.
    #[test]
    fn test_stubgen_class_body_overloaded_assign_uses_overload_callable() {
        let actual = run_stubgen(
            r#"
from typing import overload

@overload
def process(x: int) -> int: ...

@overload
def process(x: str) -> str: ...

def process(x):
    return x

class C:
    alias = process
"#,
        );
        pretty_assertions::assert_str_eq!(
            r#"
from typing import Callable, ClassVar, Overload

from typing import overload


@overload
def process(x: int) -> int: ...


@overload
def process(x: str) -> str: ...


class C:
    alias: ClassVar[Overload[Callable[[int], int], Callable[[str], str]]] = ...
"#
            .trim(),
            actual.trim(),
        );
    }
}
