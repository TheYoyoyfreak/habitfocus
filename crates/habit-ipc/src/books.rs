//! The `books` request: the book library that habitd keeps in `books.db`
//! (types in `habit_books::api`).

pub use habit_books::api::{BooksOp, BooksReply, ImportCounts};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;
    use habit_books::{Book, Status};

    #[test]
    fn requests_roundtrip() {
        let ops = [
            BooksOp::Library,
            BooksOp::List { status: Some(Status::Reading), query: "herb".into() },
            BooksOp::Save { book: Box::new(Book::new("Dune")) },
            BooksOp::Assign { pending: "p".into(), book: "b".into(), page_to: Some(3) },
            BooksOp::Pin { book: None },
            BooksOp::ImportBooktrack { path: None },
            BooksOp::Resync,
        ];
        for op in ops {
            let req = Request::Books { op };
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
            assert!(req.is_books() && !req.is_sync());
        }
        let req: Request = serde_json::from_str(r#"{"cmd":"books","op":{"op":"list"}}"#).unwrap();
        assert_eq!(req, Request::Books { op: BooksOp::List { status: None, query: String::new() } });
    }

    #[test]
    fn empty_reply_is_small() {
        assert_eq!(serde_json::to_string(&BooksReply::default()).unwrap(), "{}");
    }
}
