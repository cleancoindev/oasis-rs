#![feature(proc_macro_hygiene)]

use std::collections::{HashMap, HashSet}; // `use` statements can go anywhere

#[oasis_std::service]
mod service {
    pub type UserId = Address;
    pub type PostId = usize;

    #[derive(Service)]
    pub struct MessageBoard {
        /// The administrators of this message board.
        /// Administrators can add and remove users.
        admins: HashSet<UserId>,

        /// The maximum number of characters in a post. For instance, 280.
        bcast_char_limit: Option<u32>,

        /// All of the posts made to this message board.
        posts: Vec<Post>,

        /// All accounts which have ever participated in this message board.
        accounts: HashMap<UserId, Account>, // Strictly speaking, a `Vec` would probably be faster
                                            // since it'd be short lived and algorithmically has
                                            // smaller constant factors. The API of `HashMap` is
                                            // better suited for this field, though.
    }

    // Types used in the state struct must derive serde `Serialize` and `Deserialize`
    // so that they can be persisted and loaded from storage. They must also derive `Clone`
    // (and optionally `Copy`) so that service RPC methods can accept borrowed data which
    // improves deserialization performance.
    //
    // Types do not need to be defined in the same module as the service.
    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct Post {
        author: UserId,
        text: String,
        comments: Vec<Message>,
    }

    #[derive(Serialize, Deserialize, Clone, Default)]
    pub struct Account {
        inbox: Vec<Message>,
    }

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct Message {
        from: UserId,
        text: String,
    }

    // An event is a struct that derives `Event`. Calling `Event::emit` on the struct
    // will generate a log that can be picked up by anyone watching the blockchain.
    // Annotating a field with `#[indexed]` will allow clients to efficiently search for
    // events with a particular value of the indexed field. Up to three events may be indexed.
    //
    // A confidential application will want to encrypt the contents of the event to a
    // particular recipent.
    // A highly confidential application will probably not want to emit events at all since
    // the fact that an event was even emitted can leak information. (It can be done using
    // techniques from oblivious transfer, but it requires extreme care to do properly.)
    #[derive(Serialize, Deserialize, Event)]
    pub struct MessagePosted {
        #[indexed]
        pub author: UserId,
        #[indexed]
        pub recipient: Option<UserId>,
    }

    impl MessageBoard {
        /// Constructs a new `MessageBoard`.
        pub fn new(ctx: &Context, bcast_char_limit: Option<u32>) -> Result<Self> {
            let mut admins = HashSet::new();
            admins.insert(ctx.sender());

            let mut accounts = HashMap::new();
            accounts.insert(ctx.sender(), Account::default());

            Ok(Self {
                admins,
                bcast_char_limit,
                accounts,
                posts: Vec::new(),
            })
        }

        /// Adds a user to this message board.
        /// Can only be used by an admin.
        pub fn add_user(&mut self, ctx: &Context, user_id: UserId) -> Result<()> {
            if !self.admins.contains(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }
            self.accounts.entry(user_id).or_default();
            Ok(())
        }

        /// Removes a user from this message board.
        /// Can only be used by an admin.
        pub fn remove_user(&mut self, ctx: &Context, user_id: UserId) -> Result<()> {
            if !self.admins.contains(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }
            self.accounts.remove(&user_id);
            Ok(())
        }
    }

    impl MessageBoard {
        /// Make a post to this message board. Only registered accounts may post.
        /// Returns the id of the post.
        pub fn post(&mut self, ctx: &Context, text: String) -> Result<usize> {
            if !self.accounts.contains_key(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }

            if let Some(bcast_char_limit) = self.bcast_char_limit {
                if text.len() > bcast_char_limit as usize {
                    return Err(failure::format_err!(
                        "Message of {} characters exeeds limit of {}.",
                        text.len(),
                        bcast_char_limit
                    ));
                }
            }

            self.posts.push(Post {
                author: ctx.sender(),
                text,
                comments: Vec::new(),
            });

            Event::emit(&MessagePosted {
                author: ctx.sender(),
                recipient: None,
            });

            Ok(self.posts.len() - 1)
        }

        /// Returns all posts made during a given interval.
        pub fn posts(
            &self,
            ctx: &Context,
            range: (Option<PostId>, Option<PostId>),
        ) -> Result<Vec<Post>> {
            if !self.accounts.contains_key(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }
            let start = range.0.unwrap_or_default();
            let stop = std::cmp::min(range.1.unwrap_or(self.posts.len()), self.posts.len());
            Ok(self
                .posts
                .get(start..stop)
                .map(<[Post]>::to_vec)
                .unwrap_or_default())
        }

        /// Add a comment to a post.
        pub fn comment(&mut self, ctx: &Context, post_id: PostId, text: String) -> Result<()> {
            if !self.accounts.contains_key(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }
            if post_id >= self.posts.len() {
                return Err(failure::format_err!("Invalid post ID."));
            }
            self.posts[post_id].comments.push(Message {
                from: ctx.sender(),
                text,
            });
            Ok(())
        }
    }

    impl MessageBoard {
        pub fn send_dm(&mut self, ctx: &Context, recipient: UserId, text: String) -> Result<()> {
            if !self.accounts.contains_key(&ctx.sender()) {
                return Err(failure::format_err!("Permission denied."));
            }
            match self.accounts.get_mut(&recipient) {
                Some(recip) => {
                    recip.inbox.push(Message {
                        from: ctx.sender(),
                        text,
                    });
                    Event::emit(&MessagePosted {
                        author: ctx.sender(),
                        recipient: None,
                    });
                    Ok(())
                }
                None => Err(failure::format_err!("No such user: {}", recipient)),
            }
        }

        /// Retrieves all messages from the sender's inbox. The inbox is emptied by this operation.
        pub fn fetch_inbox(&mut self, ctx: &Context) -> Result<Vec<Message>> {
            Ok(self
                .accounts
                .get_mut(&ctx.sender())
                .map(|acct| std::mem::replace(&mut acct.inbox, Vec::new()))
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a new account and a `Context` with the new account as the sender.
    fn create_account() -> (Address, Context) {
        let addr = oasis_test::create_account(0 /* initial balance */);
        let ctx = Context::default().with_sender(addr).with_gas(100_000);
        (addr, ctx)
    }

    #[test]
    fn post_nolimit() {
        let (_admin, actx) = create_account();
        let mut mb = MessageBoard::new(&actx, None).unwrap();
        mb.post(&actx, "👏".repeat(9999)).unwrap();
    }

    #[test]
    fn post_limit() {
        let (_admin, actx) = create_account();
        let mut mb = MessageBoard::new(&actx, Some(1)).unwrap();
        mb.post(&actx, "?!".to_string()).unwrap_err();
        mb.post(&actx, "!".to_string()).unwrap();
    }

    #[test]
    fn posts() {
        let (admin, actx) = create_account();
        let (user, uctx) = create_account();

        let mut mb = MessageBoard::new(&actx, Some(140)).unwrap();

        let first_post_text = "f1r5t p0st!!1!".to_string();
        mb.post(&actx, first_post_text.clone()).unwrap();

        // no permission
        mb.post(&uctx, "Add me plz".to_string()).unwrap_err();
        mb.posts(&uctx, (None, None)).unwrap_err();
        mb.add_user(&uctx, user).unwrap_err();

        assert_eq!(
            mb.posts(&actx, (Some(0), Some(9))).unwrap(),
            vec![Post {
                author: admin,
                text: first_post_text,
                comments: Vec::new()
            }]
        );

        // user can now post
        mb.add_user(&actx, user).unwrap();
        let second_post_text = "All your base are belong to me".to_string();
        mb.post(&uctx, second_post_text.clone()).unwrap();

        assert_eq!(
            mb.posts(&uctx, (Some(1), None)).unwrap(),
            vec![Post {
                author: user,
                text: second_post_text,
                comments: Vec::new()
            }]
        );

        let comment_text = "gtfo".to_string();
        mb.comment(&actx, 0 /* post_id */, comment_text.clone())
            .unwrap();

        assert_eq!(
            mb.posts(&actx, (Some(0), Some(1))).unwrap()[0].comments,
            vec![Message {
                from: admin,
                text: comment_text
            }]
            .as_slice()
        );

        mb.remove_user(&uctx, user).unwrap_err();
        mb.remove_user(&actx, user).unwrap();
        mb.post(&uctx, "Might I ask where you keep the spoons?".to_string())
            .unwrap_err();
        mb.posts(&uctx, (None, None)).unwrap_err();
    }

    #[test]
    fn dm() {
        let (kiltavi, kctx) = create_account();
        let (joe, jctx) = create_account();

        let mut mb = MessageBoard::new(&kctx, Some(140)).unwrap();

        mb.send_dm(&jctx, kiltavi, "hello".to_string()).unwrap_err();
        mb.add_user(&kctx, joe).unwrap();
        mb.send_dm(&jctx, kiltavi, "hello".to_string()).unwrap();
        mb.send_dm(&jctx, kiltavi, "can I have some eth?".to_string())
            .unwrap();

        assert_eq!(
            mb.fetch_inbox(&kctx).unwrap(),
            vec![
                Message {
                    from: joe,
                    text: "hello".to_string()
                },
                Message {
                    from: joe,
                    text: "can I have some eth?".to_string()
                }
            ]
        );
        assert_eq!(mb.fetch_inbox(&kctx).unwrap(), Vec::new());

        mb.send_dm(&kctx, joe, "No.".to_string()).unwrap();
        assert_eq!(
            mb.fetch_inbox(&jctx).unwrap(),
            vec![Message {
                from: kiltavi,
                text: "No.".to_string()
            },]
        );
        mb.send_dm(&kctx, joe, "I am a non-giver of eth.".to_string())
            .unwrap();
        assert_eq!(
            mb.fetch_inbox(&jctx).unwrap(),
            vec![Message {
                from: kiltavi,
                text: "I am a non-giver of eth.".to_string()
            },]
        );
    }
}
