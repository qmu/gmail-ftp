//! A **minimal**, dependency-free S3 XML reader/writer for the exact response/request shapes this
//! driver needs (RFD-0001 §9 thin client, no vendor SDK): the `ListBucketResult` listing, the
//! `InitiateMultipartUploadResult` upload id, and the `CompleteMultipartUpload` request body.
//!
//! This is deliberately a tiny tag-scanner, NOT a general XML parser: S3's listing/multipart
//! envelopes are flat and well-formed, so a regex-free `<tag>...</tag>` extractor over the bytes is
//! enough and avoids pulling an XML crate (which would widen the wasm dep closure). It is exercised
//! against fixture S3 XML in the crate tests. No vendor type crosses; in/out are owned DTOs +
//! `String`.

use crate::dto::{ListPage, ObjectMeta};
use crate::multipart::PartEtag;

/// Extract the text of the first `<tag>...</tag>` inside `xml` (no namespaces, no attributes). The
/// content is XML-unescaped for the five predefined entities.
fn first_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// Extract the text of the first `<tag>` inside the given `slice`, returning an owned, unescaped
/// `String` (or empty if absent).
fn tag_text(slice: &str, tag: &str) -> String {
    first_tag(slice, tag).map(unescape).unwrap_or_default()
}

/// Iterate every `<tag>...</tag>` block in `xml`, yielding each block's inner slice. Used to walk
/// the repeated `<Contents>` / `<CommonPrefixes>` elements of a listing.
fn each_block<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel_start) = xml[cursor..].find(&open) {
        let start = cursor + rel_start + open.len();
        let Some(rel_end) = xml[start..].find(&close) else {
            break;
        };
        let end = start + rel_end;
        out.push(&xml[start..end]);
        cursor = end + close.len();
    }
    out
}

/// Unescape the five predefined XML entities (`&amp; &lt; &gt; &quot; &apos;`).
fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Escape the five predefined XML entities for a value written into a request body.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Parse an S3 `ListBucketResult` (the `list-type=2` listing) body into an owned [`ListPage`].
///
/// # Errors
/// A secret-free reason string if the envelope is not a recognizable `ListBucketResult`.
pub fn parse_list_objects(body: &[u8]) -> Result<ListPage, String> {
    let xml =
        std::str::from_utf8(body).map_err(|_| "listing body was not valid UTF-8".to_string())?;
    if !xml.contains("<ListBucketResult") {
        return Err("response was not a ListBucketResult".to_string());
    }
    let objects: Vec<ObjectMeta> = each_block(xml, "Contents")
        .iter()
        .map(|c| {
            let size = tag_text(c, "Size").parse::<u64>().unwrap_or(0);
            let storage_class = {
                let sc = tag_text(c, "StorageClass");
                if sc.is_empty() {
                    "STANDARD".to_string()
                } else {
                    sc
                }
            };
            let mut meta = ObjectMeta::new(tag_text(c, "Key"), size)
                .with_etag(tag_text(c, "ETag"))
                .with_last_modified(tag_text(c, "LastModified"))
                .with_storage_class(storage_class);
            // A versioned listing carries <VersionId> per Contents; thread it when present.
            let vid = tag_text(c, "VersionId");
            if !vid.is_empty() {
                meta.version_id = Some(vid);
            }
            meta
        })
        .collect();

    let common_prefixes: Vec<String> = each_block(xml, "CommonPrefixes")
        .iter()
        .map(|cp| tag_text(cp, "Prefix"))
        .filter(|p| !p.is_empty())
        .collect();

    let mut page = ListPage::new(objects).with_common_prefixes(common_prefixes);
    // IsTruncated + NextContinuationToken drive pagination.
    if tag_text(xml, "IsTruncated") == "true" {
        let token = tag_text(xml, "NextContinuationToken");
        if !token.is_empty() {
            page = page.with_next_token(token);
        }
    }
    Ok(page)
}

/// Parse an `InitiateMultipartUploadResult` body into the assigned `uploadId`.
///
/// # Errors
/// A secret-free reason string if no `<UploadId>` is present.
pub fn parse_upload_id(body: &[u8]) -> Result<String, String> {
    let xml = std::str::from_utf8(body)
        .map_err(|_| "multipart init body was not valid UTF-8".to_string())?;
    let id = tag_text(xml, "UploadId");
    if id.is_empty() {
        Err("no UploadId in the InitiateMultipartUploadResult".to_string())
    } else {
        Ok(id)
    }
}

/// Render a `CompleteMultipartUpload` request body from the ordered part receipts.
#[must_use]
pub fn render_complete_multipart(parts: &[PartEtag]) -> String {
    let mut out = String::from("<CompleteMultipartUpload>");
    for p in parts {
        out.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            p.part_number,
            escape(&p.etag)
        ));
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"<?xml version="1.0"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>my-bucket</Name>
  <Prefix>logs/</Prefix>
  <Delimiter>/</Delimiter>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>tok-abc</NextContinuationToken>
  <Contents>
    <Key>logs/a.json</Key>
    <LastModified>2026-06-23T00:00:00.000Z</LastModified>
    <ETag>"etag-a"</ETag>
    <Size>1024</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>logs/b.json</Key>
    <LastModified>2026-06-23T01:00:00.000Z</LastModified>
    <ETag>"etag-b"</ETag>
    <Size>2048</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes>
    <Prefix>logs/2026/</Prefix>
  </CommonPrefixes>
</ListBucketResult>"#;

    #[test]
    fn parses_a_listing_with_objects_prefixes_and_pagination() {
        let page = parse_list_objects(LISTING.as_bytes()).unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "logs/a.json");
        assert_eq!(page.objects[0].size, 1024);
        assert_eq!(page.objects[1].etag, "\"etag-b\"");
        assert_eq!(page.common_prefixes, vec!["logs/2026/".to_string()]);
        assert!(page.has_more());
        assert_eq!(page.next_token.as_deref(), Some("tok-abc"));
    }

    #[test]
    fn parses_versioned_listing_version_ids() {
        let xml = r#"<ListBucketResult><Contents><Key>k</Key><Size>1</Size><ETag>"e"</ETag><VersionId>v99</VersionId></Contents></ListBucketResult>"#;
        let page = parse_list_objects(xml.as_bytes()).unwrap();
        assert_eq!(page.objects[0].version_id.as_deref(), Some("v99"));
    }

    #[test]
    fn parses_upload_id() {
        let xml = r#"<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>upl-123</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(parse_upload_id(xml.as_bytes()).unwrap(), "upl-123");
    }

    #[test]
    fn renders_complete_multipart_in_part_order() {
        let body =
            render_complete_multipart(&[PartEtag::new(1, "\"e1\""), PartEtag::new(2, "\"e2\"")]);
        assert!(body.contains("<PartNumber>1</PartNumber>"));
        assert!(body.contains("<PartNumber>2</PartNumber>"));
        assert!(body.find("PartNumber>1").unwrap() < body.find("PartNumber>2").unwrap());
    }

    #[test]
    fn non_listing_body_is_a_structured_decode_error() {
        assert!(parse_list_objects(b"<Error><Code>AccessDenied</Code></Error>").is_err());
    }
}
