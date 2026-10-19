use super::activation_tests::run;
use crate::thread::vm::known_closure::scoped;

#[test]
#[cfg(not(miri))]
fn known_and_resolved_entries_match_at_every_executor_boundary() {
    let scripts = [
        "local n=0 for i=1,12 do n=n+i end return n",
        "local n=0 local function f(x) n=n+x end for i=1,12 do f(1) end return n",
        "local function f(...) return ... end local function g(...) return f(...) end return g(17)",
        "local t=setmetatable({n=2},{__add=function(a,b) return a.n+b.n end}) return t+t",
        "local ok=pcall(function() return 1+{} end) return ok and 0 or 1",
        "local n=0 do local x <close> = setmetatable({}, {__close=function() n=n+1 end}) end return n",
        "local n=0 for i=1,4 do n=boundary(n) end return n",
        "local ticks=0 debug.sethook(function() ticks=ticks+1 end,'',1) local n=0 for i=1,4 do n=n+i end debug.sethook() return n",
    ];
    for (case, source) in scripts.into_iter().enumerate() {
        for native in [false, true] {
            for budget in [0, 1, 17, 4096] {
                let (expected, reference) = scoped(true, || run(source, budget, native));
                let (actual, known) = scoped(false, || run(source, budget, native));
                assert_eq!(
                    expected, actual,
                    "case={case} native={native} budget={budget}"
                );
                assert_eq!(reference.0, 0);
                assert!(reference.1 > 0);
                assert_eq!(known.1, 0);
                assert!(known.0 > 0);
            }
        }
    }
}
