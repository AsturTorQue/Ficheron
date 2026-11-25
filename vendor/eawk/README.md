# eawk

An AWK interpreter in Rust that is both an embeddable library and a standalone
command. **Zero dependencies** — `cargo tree` lists exactly one crate, this one.

```console
$ cargo install eawk
$ eawk '{ print $1 }' /var/log/syslog | sort | uniq -c | sort -rn | head
```

## Two ways to use it

### As a command

```console
$ eawk 'NR > 1 && $3 > 100 { n++ } END { print n+0 }' sales.txt
```

Supports the usual flags: `-f progfile`, `-F fs`, `-v var=value`, `-` for
standard input, and `--help` / `--version`.

### As a library

Run awk code, read results back, and call across the boundary — no files, no
subprocess, no shell escaping.

```rust
use eawk::{Engine, Runner, Value};

// Run a program and read its variables back.
let mut e = Engine::new("END { print total }")?;
e.set_var("total", 42.0);
e.run()?;
assert_eq!(e.get_var("total").to_num(), 42.0);

// Feed input and capture output in memory.
let mut e = Engine::new("{ print $1 + $2 }")?;
assert_eq!(e.exec_captured("1 2\n3 4\n")?, "3\n7\n");

// Call an awk function directly.
let mut e = Engine::new("function fact(n) { return n <= 1 ? 1 : n * fact(n-1) }")?;
assert_eq!(e.call_num("fact", &[Value::from(10.0)])?, 3628800.0);

// Expose a Rust function to awk.
let mut e = Engine::new("BEGIN { print hostdouble(21) }")?;
e.register_fn1("hostdouble", |v: Value| Ok(v.to_num() * 2.0))?;
assert_eq!(e.run_captured()?, "42\n");

// Stream records one at a time — from a socket, a queue, a database cursor.
let mut r = Runner::new("{ total += $1 } END { print total }")?;
r.record("10\n")?;
r.record("20\n")?;
r.finish()?;
assert_eq!(r.output(), "30\n");
# Ok::<(), eawk::Error>(())
```

`Interp` compiles once and instantiates many times, so the same program can be
run over many inputs without re-parsing and without state leaking between runs:

```rust
# use eawk::Interp;
let interp = Interp::new("END { print NR }")?;
for chunk in ["a\nb\n", "c\n"] {
    let mut e = interp.instantiate();
    e.capture_output();          // otherwise output goes to stdout
    e.push_input(chunk)?;
    e.run()?;
    print!("{}", e.captured().unwrap());
}
# Ok::<(), eawk::Error>(())
```

## What it implements

Patterns and actions (including ranges), `BEGIN`/`END`, user functions with
recursion and the extra-parameter locals convention, associative and
multi-dimensional arrays (`a[i,j]` is `a[i SUBSEP j]`), the full expression
grammar, field splitting with the `FS` special cases, `getline`, `sub`/`gsub`,
`split`, `match`, `sprintf`, the string and maths builtins, and output
redirection.

## Compatibility

eawk targets the awk that BWK, gawk and mawk **agree** on. Its behaviour was
checked against all three by `tests/diff_oracle.sh`, which runs a corpus
through every awk on the machine and reports disagreements — a three-way
oracle, not a single reference.

The `Value` type models awk's string/number duality with four variants rather
than two, because a two-variant type silently returns wrong answers on
numeric-looking text:

```awk
BEGIN { $0="10"; $1="9"; print ($1 > $0) }   # prints 0 — a *string* comparison
BEGIN { print ("0" ? 1 : 0) }                 # prints 1 — the string "0" is true
BEGIN { print (0   ? 1 : 0) }                 # prints 0 — the number 0 is false
```

Two deliberate differences:

- **Division by zero is an error** by default, as in BWK awk and mawk. Set
  `Options::div_zero = DivZeroPolicy::Inf` for gawk's behaviour.
- **`\d`, `\w` and `\s` are not supported** in regular expressions. They are
  gawk extensions; POSIX — which BWK awk and mawk follow — treats `\w` as a
  literal `w`.

The regular-expression engine is a POSIX leftmost-*longest* NFA simulation,
written from scratch rather than pulled in as a dependency. It has no
catastrophic backtracking, which matters when a host is running untrusted awk
programs.

## Safety

The interpreter never panics on bad input. Deep expression nesting, runaway
recursion, division by zero, and invalid regular expressions all surface as
`Err(Error)` with a source position. Control flow uses an explicit `Flow` enum
rather than panics or `catch_unwind`, so a host is not left with a poisoned
panic hook.

## Development

```console
cargo test                # unit, API and doctests
./tests/diff_oracle.sh    # differential against every awk on the machine
cargo clippy --all-targets
cargo tree | wc -l        # must print 1
```

## License

Apache-2.0. See [LICENSE](LICENSE).