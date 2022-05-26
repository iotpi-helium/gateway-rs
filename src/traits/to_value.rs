use crate::{Error, Result};
use helium_proto::BlockchainVarV1;

pub trait ToValue<T> {
    fn to_value(&self) -> Result<T>;
}

#[macro_export]
macro_rules! impl_to_value {
    ($res_type:ty, $type_str:expr, $target_type:ty) => {
        impl $crate::traits::ToValue<$res_type> for $target_type {
            fn to_value(&self) -> Result<$res_type> {
                let name = &self.name;
                if self.r#type != $type_str {
                    return Err(Error::custom(format!(
                        "not an {} variable: {name}",
                        $type_str
                    )));
                }
                let value = std::str::from_utf8(&self.value)
                    .map_err(|_| Error::custom(format!("not a valid value: {name}")))
                    .and_then(|v| {
                        v.parse::<$res_type>().map_err(|_| {
                            Error::custom(format!("not a valid {} value: {name}", $type_str))
                        })
                    })?;
                Ok(value)
            }
        }
    };
}

pub(crate) use impl_to_value;

impl_to_value!(u64, "int", BlockchainVarV1);
