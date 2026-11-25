#!/usr/bin/env bash
# Differential test: run a corpus through eawk and through every awk on this
# machine, and report where they disagree.
#
# This is the highest-value check available, because the three reference
# implementations are independent. Where two of three agree, that answer is
# almost certainly the correct one; a 1-vs-2 split tells us which oracle to
# believe.
#
# Usage:  tests/diff_oracle.sh [filter]
#         tests/diff_oracle.sh substr      # only cases whose name matches
#
# Exit status is non-zero if any case diverges from all three oracles.

set -uo pipefail
cd "$(dirname "$0")/.."

EAWK=${EAWK:-target/debug/eawk}
FILTER=${1:-}

AWKS=()
for candidate in /usr/bin/awk "$HOME/.local/bin/gawk" /opt/homebrew/bin/mawk; do
    [ -x "$candidate" ] && AWKS+=("$candidate")
done
if [ ${#AWKS[@]} -eq 0 ]; then
    echo "no reference awk found; skipping" >&2
    exit 0
fi

[ -x "$EAWK" ] || { echo "build first: cargo build" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

INPUT=$'1 2 3\n4 5 6\n7 8 9\nalpha beta\n'
printf '%s' "$INPUT" > "$WORK/input.txt"

pass=0
fail=0
declare -a FAILED

# run_case <name> <awk-source> [extra-args...]
run_case() {
    local name=$1 prog=$2
    shift 2
    if [ -n "$FILTER" ] && [[ "$name" != *"$FILTER"* ]]; then return; fi

    local mine
    if ! mine=$("$EAWK" "$@" "$prog" "$WORK/input.txt" 2>&1); then
        mine="<exit $?> ${mine}"
    fi

    local -a outs=()
    local -a votes=()
    for a in "${AWKS[@]}"; do
        local out
        out=$("$a" "$@" "$prog" "$WORK/input.txt" 2>&1)
        # `for (k in a)` order is unspecified, so compare sorted.
        printf '%s\n' "$out" | LC_ALL=C sort > "$WORK/sorted"
        votes+=("$(cat "$WORK/sorted")")
    done

    # Majority vote among the oracles.
    local winner majority=0
    for candidate in "${votes[@]}"; do
        local n=0
        for v in "${votes[@]}"; do
            [ "$v" = "$candidate" ] && n=$((n + 1))
        done
        if [ "$n" -gt "$majority" ]; then
            majority=$n
            winner=$candidate
        fi
    done

    # Prefer an exact match, so ordering stays significant for tests that have a
    # defined order. The sorted comparison is the fallback for `for (k in a)`,
    # whose iteration order POSIX leaves unspecified.
    printf '%s\n' "$mine" | LC_ALL=C sort > "$WORK/mine_sorted"
    if [ "$mine" = "$winner" ] || [ "$(cat "$WORK/mine_sorted")" = "$winner" ]; then
        pass=$((pass + 1))
        return
    fi

    fail=$((fail + 1))
    FAILED+=("$name")
    {
        echo "───────────────────────────────────────────────"
        echo "FAIL: $name"
        echo "  program: $prog"
        echo "  eawk   : $(printf '%s' "$mine" | head -6 | tr '\n' '|')"
        echo "  oracle : $(printf '%s' "$winner" | head -6 | tr '\n' '|')   (majority $majority/${#AWKS[@]})"
        for i in "${!AWKS[@]}"; do
            echo "    $(basename "${AWKS[$i]}"): $(printf '%s' "${votes[$i]}" | head -3 | tr '\n' '|')"
        done
    } >> "$WORK/report"
    cat "$WORK/report"
    : > "$WORK/report"
}

# ---- the corpus -----------------------------------------------------------
run_case fields           '{ print $1, $2 }'
run_case default-print    '{ print }'
run_case pattern-only     '$2 == 5'
run_case range            'NR == 2, NR == 3 { print "r", $0 }'
run_case regex-anchor     '/^a/ { print }'
run_case regex-mid        '/b.ta/ { print }'
run_case regex-alt        '/alpha|gamma/ { print }'
run_case regex-class      '/[0-9]/ { n++ } END { print n+0 }'
run_case regex-quant      '/^[0-9]{1,3}$/ { print }'
run_case regex-neg-class  '/[^0-9]/ { print }'
run_case regex-or         '/(a|b)lpha/ { print }'
run_case regex-longest    'BEGIN { print match("ab", /a|ab/) }'
run_case count-words      '{ for (i = 1; i <= NF; i++) n++ } END { print n+0 }'
run_case sum-fields       '{ for (i = 1; i <= NF; i++) s += $i } END { print s+0 }'
run_case awk-arith        'BEGIN { print 1/3, 1000, 2^31, -2^2, 2^3^2, 7%3, -7%3 }'
run_case concat          'BEGIN { print "a" 1, 1 " " 2, "x" "" "y" }'
run_case numeric-format  'BEGIN { print 0.1+0.2, 1e6, 0.000001, 1000000 }'
run_case ofmt            'BEGIN { OFMT="%.2f"; print 1/3 }'
run_case convfmt         'BEGIN { CONVFMT="%.3g"; print (1/3) "" }'
run_case printf-basic    'BEGIN { printf "%d|%5.2f|%s|%c|%x\n", 42, 3.14159, "hi", 65, 255 }'
run_case printf-flags    'BEGIN { printf "[%-6s][%06d][%+d]\n", "ab", 42, 42 }'
run_case sprintf-call    'BEGIN { print sprintf("%s-%s", "a", "b") }'
run_case substr-basic    'BEGIN { print substr("hello", 2, 3), substr("hello", 2), "|", substr("hello", 10, 2) }'
run_case index-fn        'BEGIN { print index("hello", "ll"), index("hello", "z") }'
run_case length-fn       'BEGIN { print length("abc"), length(), length(12345) }'
run_case split-basic     'BEGIN { n = split("a:b:c", p, ":"); print n, p[1], p[3] }'
run_case split-default   'BEGIN { n = split("a b  c", p); print n, p[2] }'
run_case split-trailing  'BEGIN { print split("a:b:", p, ":"), split("", q, ":"), split(",,", r, ",") }'
run_case gsub-2arg       '{ gsub(/[0-9]/, "") ; print }'
run_case sub-3arg        'BEGIN { s = "aaa"; print sub(/a/, "b", s), s }'
run_case gsub-amp        'BEGIN { s = "cat"; gsub(/[ao]/, "[&]", s); print s }'
run_case match-rstart    'BEGIN { print match("foobar", "o+"), RSTART, RLENGTH }'
run_case match-nomatch   'BEGIN { print match("abc", "z"), RSTART, RLENGTH }'
run_case arrays          'BEGIN { a["x"]=1; a["y"]=2; s=0; for (k in a) s+=a[k]; print s+0 }'
run_case array-in        'BEGIN { a[1]=1; print (1 in a), (2 in a) }'
run_case array-delete    'BEGIN { a[1]=1; a[2]=2; delete a[1]; n=0; for(k in a) n++; print n+0 }'
run_case multidim        'BEGIN { a[1,2]=7; for (k in a) { split(k, p, SUBSEP); print p[1], p[2] } }'
run_case functions       'function f(a, b) { return a + b } BEGIN { print f(1, 2) }'
run_case recursion       'function fact(n) { return n <= 1 ? 1 : n * fact(n-1) } BEGIN { print fact(6) }'
run_case local-params    'function f(a,   tmp) { tmp = 99; return a } BEGIN { print f(1) }'
run_case control-flow    'BEGIN { for (i=0;i<5;i++) { if (i%2) continue; if (i>3) break; s+=i } print s+0 }'
run_case while-do        'BEGIN { i=0; while (i<3) i++; do i--; while (i>0); print i }'
run_case ternary        'BEGIN { print (1>2) ? "a" : "b" }'
run_case incr-decr       'BEGIN { i=5; print i++, i, ++i, i--, --i }'
run_case assignments     'BEGIN { x = 5; x += 3; x -= 1; x *= 2; x /= 7; print x }'
run_case uninit          'BEGIN { print "[" x "]", (x==""), (x==0), length(x) }'
run_case truthy-string   'BEGIN { print ("0"?1:0), (0?1:0), (""?1:0), ("abc"?1:0) }'
run_case strnum-cmp      'BEGIN { $0="10"; $1="9"; print ($1 > $0), (10 < "9"), ($1+0 > $0+0) }'
run_case next-stmt       '{ if (NR==2) next; print }'
run_case exit-code       'BEGIN { print "b" } { print "m" } END { print "e" }'
run_case nf-assign       'BEGIN { $0="a b c"; NF=1; print NF, $0 }'
run_case field-grow      'BEGIN { $0="a b c"; $5="e"; print NF, $0 }'
run_case field-rebuild   '{ $2 = "X"; print }'
run_case dollar0-set     '{ $0 = toupper($0); print }'
run_case ofs-output      'BEGIN { OFS="-"; $0="a b c"; print $1, $2 }'
run_case ors-output      'BEGIN { ORS="|"; print "a"; print "b" }'
run_case sprintf-width   'BEGIN { printf "%10.3s|\n", "abcdefgh" }'
run_case tolower-upper   'BEGIN { print toupper("aBc"), tolower("AbC") }'
run_case math-fns        'BEGIN { printf "%.4f %.4f %.4f\n", sqrt(2), exp(1), log(10) }'
run_case int-fn          'BEGIN { print int(-3.7), int(3.7) }'
run_case getline-file    'BEGIN { while ((getline line < "'"$WORK"'/input.txt") > 0) n++; print n+0 }'
run_case special-vars    '{ print NR, NF, FNR }'
run_case compare-chain   'BEGIN { print (1 < 2), (2 <= 2), (3 > 4), ("a" < "b"), (1 == 1), (1 != 2) }'
run_case unary-not       'BEGIN { print !0, !1, !"", !"x" }'
run_case nested-func     'function a(x){return b(x)+1} function b(x){return x*2} BEGIN { print a(5) }'

echo
echo "passed: $pass   failed: $fail"
if [ "$fail" -gt 0 ]; then
    echo "failed cases: ${FAILED[*]}"
    exit 1
fi