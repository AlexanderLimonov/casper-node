use casper_types::bytesrepr::{self, Bytes, FromBytes, ToBytes};
#[cfg(test)]
use casper_types::testing::TestRng;
#[cfg(test)]
use rand::Rng;

#[derive(Debug, PartialEq)]
pub(crate) struct OriginalRequestSpec {
    id: u64,
    data: Vec<u8>,
}

impl OriginalRequestSpec {
    pub(crate) fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }

    #[cfg(test)]
    pub(crate) fn random(rng: &mut TestRng) -> Self {
        Self {
            id: rng.gen(),
            data: rng.random_vec(64..128),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }
}

impl ToBytes for OriginalRequestSpec {
    fn to_bytes(&self) -> Result<Vec<u8>, bytesrepr::Error> {
        let mut buffer = bytesrepr::allocate_buffer(self)?;
        self.write_bytes(&mut buffer)?;
        Ok(buffer)
    }

    fn write_bytes(&self, writer: &mut Vec<u8>) -> Result<(), bytesrepr::Error> {
        let OriginalRequestSpec { id, data } = self;

        id.write_bytes(writer)?;
        data.write_bytes(writer)
    }

    fn serialized_length(&self) -> usize {
        self.id.serialized_length() + self.data.serialized_length()
    }
}

impl FromBytes for OriginalRequestSpec {
    fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), bytesrepr::Error> {
        let (id, remainder) = FromBytes::from_bytes(bytes)?;
        let (data, remainder) = Bytes::from_bytes(remainder)?;

        Ok((
            OriginalRequestSpec {
                id,
                data: data.into(),
            },
            remainder,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use casper_types::testing::TestRng;

    #[test]
    fn bytesrepr_roundtrip() {
        let rng = &mut TestRng::new();

        let val = OriginalRequestSpec::random(rng);
        bytesrepr::test_serialization_roundtrip(&val);
    }
}
