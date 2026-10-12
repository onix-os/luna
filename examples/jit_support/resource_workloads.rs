pub const RELOCATIONS: &[u8] =
    b"local t={} for i=1,21 do t[i]=2 end local s=0 for i=1,21 do s=s+t[i] end return s";

pub const SCALAR_VERIFIER: &[u8] = b"local s=21 for i=1,6 do s=s+i end return s";
