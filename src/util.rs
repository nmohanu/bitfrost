pub fn bitwise_and(a: &Box<[bool]>, b: &Box<[bool]>) -> Box<[bool]> {
    let len = std::cmp::min(a.len(), b.len());
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        result.push(a[i] && b[i]);
    }
    result.into_boxed_slice()
}

pub fn vec_u8_to_box_bool(vec: Vec<u8>) -> Box<[bool]> {
    let mut bools = Vec::with_capacity(vec.len() * 8);

    for byte in vec {
        for i in (0..8).rev() { 
            bools.push((byte >> i) & 1 != 0);
        }
    }

    bools.into_boxed_slice()
}