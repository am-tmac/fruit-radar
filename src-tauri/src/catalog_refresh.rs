//! Independent catalog endpoints: a failure must not erase the other's success.
use apw_core::catalog::{Catalog, CatalogError};
use apw_core::model::{Category, Region};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogRefresh {
    pub products: usize,
    pub stores: Option<usize>,
    pub errors: Vec<String>,
}

fn summarize(
    stores: Result<usize, CatalogError>,
    products: Result<usize, CatalogError>,
) -> Result<CatalogRefresh, String> {
    let mut errors = Vec::new();
    let stores = match stores {
        Ok(count) => Some(count),
        Err(error) => {
            errors.push(format!("门店列表：{error}"));
            None
        }
    };
    let products = match products {
        Ok(count) => count,
        Err(error @ CatalogError::RefreshFailed { .. }) => {
            let count = match &error {
                CatalogError::RefreshFailed { fetched, .. } => *fetched,
                _ => unreachable!(),
            };
            errors.push(format!("型号目录：{error}"));
            count
        }
        Err(error) => {
            errors.push(format!("型号目录：{error}"));
            0
        }
    };
    if stores.is_none() && products == 0 {
        return Err(errors.join("；"));
    }
    Ok(CatalogRefresh {
        products,
        stores,
        errors,
    })
}

pub(crate) async fn refresh(
    catalog: &Catalog,
    region: &'static Region,
    category: Option<Category>,
    http: &reqwest::Client,
) -> Result<CatalogRefresh, String> {
    // Endpoints remain independent; no early ? can suppress a successful update.
    let (stores, products) = tokio::join!(
        catalog.refresh_stores(region, http),
        catalog.refresh_products(region, category, http)
    );
    summarize(stores, products)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apw_core::catalog::CatalogError;

    #[test]
    fn store_failure_preserves_successful_products() {
        let result = summarize(
            Err(CatalogError::PageSchema {
                detail: "offline stores".into(),
            }),
            Ok(7),
        )
        .unwrap();
        assert_eq!(result.products, 7);
        assert_eq!(result.stores, None);
        assert!(result.errors[0].contains("offline stores"));
    }

    #[test]
    fn product_failure_preserves_successful_stores() {
        let result = summarize(
            Ok(49),
            Err(CatalogError::RefreshFailed {
                locale: "zh_CN".into(),
                fetched: 0,
                failures: vec!["offline products".into()],
            }),
        )
        .unwrap();
        assert_eq!(result.products, 0);
        assert_eq!(result.stores, Some(49));
        assert!(result.errors[0].contains("offline products"));
    }

    #[test]
    fn partial_products_are_reported_and_complete_failure_is_an_error() {
        let result = summarize(
            Ok(49),
            Err(CatalogError::RefreshFailed {
                locale: "zh_CN".into(),
                fetched: 3,
                failures: vec!["broken page".into()],
            }),
        )
        .unwrap();
        assert_eq!(result.products, 3);
        assert_eq!(result.stores, Some(49));
        assert!(result.errors[0].contains("broken page"));
        let error = summarize(
            Err(CatalogError::PageSchema {
                detail: "stores failed".into(),
            }),
            Err(CatalogError::PageSchema {
                detail: "products failed".into(),
            }),
        )
        .unwrap_err();
        assert!(error.contains("stores failed"));
        assert!(error.contains("products failed"));
    }
}
