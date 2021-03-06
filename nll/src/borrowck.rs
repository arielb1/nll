use env::{Environment, Point};
use loans_in_scope::{Loan, LoansInScope};
use nll_repr::repr;
use std::error::Error;
use std::fmt;

pub fn borrow_check(env: &Environment, loans_in_scope: &LoansInScope) -> Result<(), Box<Error>> {
    let mut result: Result<(), Box<Error>> = Ok(());
    loans_in_scope.walk(env, |point, opt_action, loans| {
        let borrowck = BorrowCheck { env, point, loans };
        if let Some(action) = opt_action {
            if let Err(e) = borrowck.check_action(action) {
                if !action.should_have_error {
                    result = Err(e);
                }
            } else if action.should_have_error {
                result = Err(Box::new(BorrowError::no_error(point)));
            }
        }
    });

    result
}

struct BorrowCheck<'cx> {
    env: &'cx Environment<'cx>,
    point: Point,
    loans: &'cx [&'cx Loan<'cx>],
}

impl<'cx> BorrowCheck<'cx> {
    fn check_action(&self, action: &repr::Action) -> Result<(), Box<Error>> {
        log!("check_action({:?}) at {:?}", action, self.point);
        match action.kind {
            repr::ActionKind::Init(ref a, ref bs) => {
                self.check_write(a)?;
                for b in bs {
                    self.check_read(b)?;
                }
            }
            repr::ActionKind::Assign(ref a, ref b) => {
                self.check_write(a)?;
                self.check_read(b)?;
            }
            repr::ActionKind::Borrow(ref a, _, repr::BorrowKind::Shared, ref b) => {
                self.check_write(a)?;
                self.check_read(b)?;
            }
            repr::ActionKind::Borrow(ref a, _, repr::BorrowKind::Mut, ref b) => {
                self.check_write(a)?;
                self.check_read(b)?;
                self.check_write(b)?;
            }
            repr::ActionKind::Constraint(_) => {}
            repr::ActionKind::Use(ref p) => {
                self.check_read(p)?;
            }
            repr::ActionKind::Drop(ref p) => {
                self.check_move(p)?;
            }
            repr::ActionKind::StorageDead(p) => {
                self.check_storage_dead(p)?;
            }
            repr::ActionKind::Noop => {}
        }

        Ok(())
    }

    /// Cannot write to a path `p` if:
    /// - the path `p` is frozen by one of the loans
    ///   - see `frozen_by_borrow_of()`
    ///   - this covers writing to `a` (or `a.b`) when `a.b` is borrowed
    /// - some prefix of `p` is borrowed.
    ///   - this covers writing to `a.b.c` when `a.b` is borrowed
    fn check_write(&self, path: &repr::Path) -> Result<(), Box<Error>> {
        log!(
            "check_write of {:?} at {:?} with loans={:#?}",
            path,
            self.point,
            self.loans
        );
        if let Some(loan) = self.find_loan_that_freezes(path) {
            return Err(Box::new(BorrowError::for_write(
                self.point,
                path,
                &loan.path,
                loan.point,
            )));
        }
        Ok(())
    }

    /// Cannot read from a path `a.b.c` if:
    /// - the exact path `a.b.c` is borrowed mutably;
    /// - some subpath `a.b.c.d` is borrowed mutably;
    /// - some prefix of `a.b` is borrowed mutably.
    fn check_read(&self, path: &repr::Path) -> Result<(), Box<Error>> {
        log!(
            "check_read of {:?} at {:?} with loans={:#?}",
            path,
            self.point,
            self.loans
        );
        for loan in self.find_loans_that_intersect(path) {
            match loan.kind {
                repr::BorrowKind::Shared => { /* Ok */ }
                repr::BorrowKind::Mut => {
                    return Err(Box::new(BorrowError::for_read(
                        self.point,
                        path,
                        &loan.path,
                        loan.point,
                    )));
                }
            }
        }
        Ok(())
    }

    /// Cannot move from a path `p` if:
    /// - `p` is borrowed;
    /// - some subpath `p.foo` is borrowed;
    /// - some prefix of `p` is borrowed.
    ///
    /// Note that this is stricter than both *writes* and
    /// *storage-dead*. In particular, you **can** write to a variable
    /// `x` that contains an `&mut` value when `*x` is borrowed, but
    /// you **cannot** move `x`. This is because moving it would make
    /// the `&mut` available in the new location, but writing (and
    /// storage-dead) both kill it forever.
    fn check_move(&self, path: &repr::Path) -> Result<(), Box<Error>> {
        log!(
            "check_move of {:?} at {:?} with loans={:#?}",
            path,
            self.point,
            self.loans
        );
        for loan in self.find_loans_that_intersect(path) {
            return Err(Box::new(BorrowError::for_move(
                self.point,
                path,
                &loan.path,
                loan.point,
            )));
        }
        Ok(())
    }

    /// Cannot free a local variable `var` if:
    /// - data interior to `var` is borrowed.
    ///
    /// In particular, having something like `*var` borrowed is ok.
    fn check_storage_dead(&self, var: repr::Variable) -> Result<(), Box<Error>> {
        log!(
            "check_storage_dead of {:?} at {:?} with loans={:#?}",
            var,
            self.point,
            self.loans
        );
        if let Some(loan) = self.find_loan_that_freezes(&repr::Path::Var(var)) {
            return Err(Box::new(BorrowError::for_storage_dead(
                self.point,
                var,
                &loan.path,
                loan.point,
            )));
        }
        Ok(())
    }

    /// A loan L *intersects* a path P if either:
    ///
    /// - the loan is for the path P; or,
    /// - the path P can be extended to reach the data in the loan; or,
    /// - the loan path can be extended to reach the data in P.
    ///
    /// So, for example, is the path P is `a.b.c`, then:
    ///
    /// - a loan of `a.b.c` intersects P;
    /// - a loan of `a.b.c.d` intersects P, because (e.g.) after reading P
    ///   you have also read `a.b.c.d`;
    /// - a loan of `a.b` intersects P, because you can use the
    ///   reference to access the data at P.
    fn find_loans_that_intersect<'a>(
        &'a self,
        path: &'a repr::Path,
    ) -> impl Iterator<Item = &'a Loan> + 'a {
        let path_prefixes = path.prefixes();
        self.loans.iter().cloned().filter(move |loan| {
            // accessing `a.b.c` intersects a loan of `a.b.c` or `a.b`...
            path_prefixes.contains(&loan.path) ||

            // ...as well as a loan of `a.b.c.d`
                self.env.supporting_prefixes(&loan.path).contains(&path)
        })
    }

    /// Helper for `check_write` and `check_storage_dead`: finds if
    /// there is a loan that "freezes" the given path -- that is, a
    /// loan that would make modifying the `path` (or freeing it)
    /// illegal. This is slightly more permissive than the rules
    /// around move and reads, precisely because overwriting or
    /// freeing `path` makes the previous value unavailable from that
    /// point on.
    fn find_loan_that_freezes(&self, path: &repr::Path) -> Option<&Loan> {
        let prefixes = path.prefixes();
        self.loans.iter().cloned().find(|loan| {
            // If you have borrowed `a.b`, this prevents writes to `a`
            // or `a.b`:
            let frozen_paths = self.frozen_by_borrow_of(&loan.path);
            frozen_paths.contains(&path) ||

                // If you have borrowed `a.b`, this prevents writes to
                // `a.b.c`:
                prefixes.contains(&loan.path)
        })
    }

    /// If `path` is mutably borrowed, returns a vector of paths which -- if
    /// moved or if the storage went away -- would invalidate this
    /// reference.
    fn frozen_by_borrow_of<'a>(&self, mut path: &'a repr::Path) -> Vec<&'a repr::Path> {
        let mut result = vec![];
        loop {
            result.push(path);
            match *path {
                repr::Path::Var(_) => return result,
                repr::Path::Extension(ref base_path, field_name) => {
                    match *self.env.path_ty(base_path) {
                        // If you borrowed `*r`, writing to `r` does
                        // not actually affect the memory at `*r`, so
                        // we can stop iterating backwards now.
                        repr::Ty::Ref(_, _, _) => {
                            assert_eq!(field_name, repr::FieldName::star());
                            return result;
                        }

                        // If you have borrowed `a.b`, then writing to
                        // `a` would overwrite `a.b`, which is
                        // disallowed.
                        repr::Ty::Struct(..) => {
                            path = base_path;
                        }

                        repr::Ty::Unit => panic!("unit has no fields"),
                        repr::Ty::Bound(..) => panic!("unexpected bound type"),
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct BorrowError {
    description: String,
}

impl BorrowError {
    fn no_error(point: Point) -> Self {
        BorrowError {
            description: format!("point {:?} had no error, but should have", point),
        }
    }

    fn for_move(
        point: Point,
        path: &repr::Path,
        loan_path: &repr::Path,
        loan_point: Point,
    ) -> Self {
        BorrowError {
            description: format!(
                "point {:?} cannot move {:?} because {:?} is borrowed (at point `{:?}`)",
                point,
                path,
                loan_path,
                loan_point
            ),
        }
    }

    fn for_read(
        point: Point,
        path: &repr::Path,
        loan_path: &repr::Path,
        loan_point: Point,
    ) -> Self {
        BorrowError {
            description: format!(
                "point {:?} cannot read {:?} because {:?} is mutably borrowed (at point `{:?}`)",
                point,
                path,
                loan_path,
                loan_point
            ),
        }
    }

    fn for_write(
        point: Point,
        path: &repr::Path,
        loan_path: &repr::Path,
        loan_point: Point,
    ) -> Self {
        BorrowError {
            description: format!(
                "point {:?} cannot write {:?} because {:?} is borrowed (at point `{:?}`)",
                point,
                path,
                loan_path,
                loan_point
            ),
        }
    }

    fn for_storage_dead(
        point: Point,
        var: repr::Variable,
        loan_path: &repr::Path,
        loan_point: Point,
    ) -> Self {
        BorrowError {
            description: format!(
                "point {:?} cannot kill storage for {:?} \
                 because {:?} is borrowed (at point `{:?}`)",
                point,
                var,
                loan_path,
                loan_point
            ),
        }
    }
}

impl Error for BorrowError {
    fn description(&self) -> &str {
        &self.description
    }

    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", self.description)
    }
}
