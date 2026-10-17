//! Compile-path microbenches (lexing stage). `tokenize` is pure Rust — the
//! parser stage below it needs a live Python `Engine`, so end-to-end compile
//! cost is covered by `benchmarks/bench.py` instead.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use _dtl::lexer::{smart_split, split_contents, tokenize};

/// ~200-node template: 40 text/var pairs + 20 block tags + a small loop nest.
fn representative_source() -> String {
    let mut s = String::new();
    for i in 0..40 {
        s.push_str("<p>row ");
        s.push_str(&i.to_string());
        s.push_str(" = {{ value");
        s.push_str(&i.to_string());
        s.push_str("|upper }}</p>\n");
    }
    s.push_str("{% if flag %}{% for x in items %}{{ x|default:'-' }}{% cycle 'a' 'b' %}{% endfor %}{% else %}none{% endif %}");
    s.push_str("{% spaceless %}<b>{% now 'Y' %}</b>{% endspaceless %}");
    s
}

fn bench_lex(c: &mut Criterion) {
    let src = representative_source();
    c.bench_function("lex/release", |b| {
        b.iter(|| tokenize(black_box(&src), false))
    });
    c.bench_function("lex/debug_positions", |b| {
        b.iter(|| tokenize(black_box(&src), true))
    });
    c.bench_function("split_contents", |b| {
        b.iter(|| split_contents(black_box("url 'named.client' a=1 b=x|upper")))
    });
    c.bench_function("smart_split", |b| {
        b.iter(|| smart_split(black_box("for i in nums|slice:'1:3' as items")))
    });
}

criterion_group!(benches, bench_lex);
criterion_main!(benches);
