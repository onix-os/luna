pub struct Workload {
    pub name: &'static str,
    pub source: &'static [u8],
    pub expected: i64,
}

pub const WORKLOADS: &[Workload] = &[
    Workload {
        name: "integer_loop",
        source: b"local sum=0 for i=1,20000 do sum=sum+i end return sum",
        expected: 200010000,
    },
    Workload {
        name: "float_loop",
        source: b"local sum=0.0 for i=1,20000 do sum=sum+0.5 end return sum==10000.0 and 10000 or -1",
        expected: 10000,
    },
    Workload {
        name: "array_table",
        source: b"local t={} for i=1,5000 do t[i]=i end local sum=0 for i=1,5000 do sum=sum+t[i] end return sum",
        expected: 12502500,
    },
    Workload {
        name: "closure_upvalue",
        source: b"local sum=0 local function add(v) sum=sum+v end for i=1,10000 do add(i) end return sum",
        expected: 50005000,
    },
    Workload {
        name: "polymorphic_metamethod",
        source: b"local mt={} mt.__add=function(a,b) return setmetatable({n=a.n+b.n},mt) end local sum=setmetatable({n=0},mt) local one=setmetatable({n=1},mt) for i=1,1000 do sum=sum+one end return sum.n",
        expected: 1000,
    },
    Workload {
        name: "rust_callbacks",
        source: b"local sum=0 for i=1,5000 do sum=sum+host_increment(i) end return sum",
        expected: 12507500,
    },
    Workload {
        name: "allocation_gc",
        source: b"local sum=0 for i=1,2000 do local t={i,i+1} sum=sum+t[1] end return sum",
        expected: 2001000,
    },
];

pub const PREDICATE_SOURCE: &[u8] = b"return free < 1e9";
pub const COLD_SOURCE: &[u8] =
    b"local config={width=40,enabled=true} return config.enabled and config.width+2 or 0";
