#![allow(dead_code, unused_imports)]

use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use quickcheck::{Arbitrary, Gen, QuickCheck, StdGen};
use rand::{self, Rng};

use super::{DirEntry, WalkDir, WalkDirError, WalkDirIter};

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
enum Tree {
    Dir(PathBuf, Vec<Tree>),
    File(PathBuf),
    Symlink(PathBuf, PathBuf),
}

impl Tree {
    fn from_walk<P: AsRef<Path>>(p: P) -> io::Result<Tree> {
        Tree::from_walk_with(p, |wd| wd)
    }

    fn from_walk_with<P, F>(
        p: P,
        f: F,
    ) -> io::Result<Tree>
    where P: AsRef<Path>, F: FnOnce(WalkDir<P>) -> WalkDir<P> {
        let mut stack = vec![Tree::Dir(p.as_ref().to_path_buf(), vec![])];
        let it: WalkEventIter = f(WalkDir::new(p)).into();
        for ev in it {
            match try!(ev) {
                WalkEvent::Exit => {
                    let tree = stack.pop().unwrap();
                    stack.last_mut().unwrap().children_mut().push(tree);
                }
                WalkEvent::Dir(dent) => {
                    stack.push(Tree::Dir(pb(dent.file_name()), vec![]));
                }
                WalkEvent::File(dent) => {
                    let node = if try!(dent.file_type()).is_symlink() {
                        let src = try!(fs::read_link(dent.path()));
                        let dst = pb(dent.file_name());
                        Tree::Symlink(src, dst)
                    } else {
                        Tree::File(pb(dent.file_name()))
                    };
                    stack.last_mut().unwrap().children_mut().push(node);
                }
            }
        }
        assert_eq!(stack.len(), 1);
        Ok(stack.pop().unwrap())
    }

    fn name(&self) -> &Path {
        match *self {
            Tree::Dir(ref pb, _) => pb,
            Tree::File(ref pb) => pb,
            Tree::Symlink(_, ref pb) => pb,
        }
    }

    fn unwrap_singleton(self) -> Tree {
        match self {
            Tree::File(_) | Tree::Symlink(_, _) => {
                panic!("cannot unwrap file as dir");
            }
            Tree::Dir(_, mut childs) => {
                assert_eq!(childs.len(), 1);
                childs.pop().unwrap()
            }
        }
    }

    fn children_mut(&mut self) -> &mut Vec<Tree> {
        match *self {
            Tree::File(_) | Tree::Symlink(_, _) => {
                panic!("files do not have children");
            }
            Tree::Dir(_, ref mut children) => children,
        }
    }

    fn create_in<P: AsRef<Path>>(&self, parent: P) -> io::Result<()> {
        let parent = parent.as_ref();
        match *self {
            Tree::Symlink(ref src, ref dst) => {
                try!(soft_link(src, parent.join(dst)));
            }
            Tree::File(ref p) => { try!(File::create(parent.join(p))); }
            Tree::Dir(ref dir, ref children) => {
                try!(fs::create_dir(parent.join(dir)));
                for child in children {
                    try!(child.create_in(parent.join(dir)));
                }
            }
        }
        Ok(())
    }

    fn canonical(&self) -> Tree {
        match *self {
            Tree::Symlink(ref src, ref dst) => {
                Tree::Symlink(src.clone(), dst.clone())
            }
            Tree::File(ref p) => {
                Tree::File(p.clone())
            }
            Tree::Dir(ref p, ref cs) => {
                let mut cs: Vec<Tree> =
                    cs.iter().map(|c| c.canonical()).collect();
                cs.sort();
                Tree::Dir(p.clone(), cs)
            }
        }
    }

    fn dedup(&self) -> Tree {
        match *self {
            Tree::Symlink(ref src, ref dst) => {
                Tree::Symlink(src.clone(), dst.clone())
            }
            Tree::File(ref p) => {
                Tree::File(p.clone())
            }
            Tree::Dir(ref p, ref cs) => {
                let mut nodupes: Vec<Tree> = vec![];
                for (i, c1) in cs.iter().enumerate() {
                    if !cs[i+1..].iter().any(|c2| c1.name() == c2.name())
                        && !nodupes.iter().any(|c2| c1.name() == c2.name()) {
                        nodupes.push(c1.dedup());
                    }
                }
                Tree::Dir(p.clone(), nodupes)
            }
        }
    }

    fn gen<G: Gen>(g: &mut G, depth: usize) -> Tree {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct NonEmptyAscii(String);

        impl Arbitrary for NonEmptyAscii {
            fn arbitrary<G: Gen>(g: &mut G) -> NonEmptyAscii {
                use std::char::from_u32;
                let upper_bound = g.size();
                // We start with a lower bound of `4` to avoid
                // generating the special file name `con` on Windows,
                // because such files cannot exist...
                let size = g.gen_range(4, upper_bound);
                NonEmptyAscii((0..size)
                .map(|_| from_u32(g.gen_range(97, 123)).unwrap())
                .collect())
            }

            fn shrink(&self) -> Box<Iterator<Item=NonEmptyAscii>> {
                let mut smaller = vec![];
                for i in 1..self.0.len() {
                    let s: String = self.0.chars().skip(i).collect();
                    smaller.push(NonEmptyAscii(s));
                }
                Box::new(smaller.into_iter())
            }
        }

        let name = pb(NonEmptyAscii::arbitrary(g).0);
        if depth == 0 {
            Tree::File(name)
        } else {
            let children: Vec<Tree> =
                (0..g.gen_range(0, 5))
                .map(|_| Tree::gen(g, depth-1))
                .collect();
            Tree::Dir(name, children)
        }
    }
}

impl Arbitrary for Tree {
    fn arbitrary<G: Gen>(g: &mut G) -> Tree {
        let depth = g.gen_range(0, 5);
        Tree::gen(g, depth).dedup()
    }

    fn shrink(&self) -> Box<Iterator<Item=Tree>> {
        let trees: Box<Iterator<Item=Tree>> = match *self {
            Tree::Symlink(_, _) => unimplemented!(),
            Tree::File(ref path) => {
                let s = path.to_string_lossy().into_owned();
                Box::new(s.shrink().map(|s| Tree::File(pb(s))))
            }
            Tree::Dir(ref path, ref children) => {
                let s = path.to_string_lossy().into_owned();
                if children.is_empty() {
                    Box::new(s.shrink().map(|s| Tree::Dir(pb(s), vec![])))
                } else if children.len() == 1 {
                    let c = &children[0];
                    Box::new(Some(c.clone()).into_iter().chain(c.shrink()))
                } else {
                    Box::new(children
                             .shrink()
                             .map(move |cs| Tree::Dir(pb(s.clone()), cs)))
                }
            }
        };
        Box::new(trees.map(|t| t.dedup()))
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fn rep(c: char, n: usize) -> String {
            ::std::iter::repeat(c).take(n).collect()
        }

        fn fmt(
            f: &mut fmt::Formatter,
            tree: &Tree,
            depth: usize,
        ) -> fmt::Result {
            match *tree {
                Tree::File(ref pb) => {
                    writeln!(f, "{}{}", rep(' ', 2 * depth), pb.display())
                }
                Tree::Symlink(ref src, ref dst) => {
                    writeln!(f, "{}{} -> {}",
                             rep(' ', 2 * depth),
                             dst.display(), src.display())
                }
                Tree::Dir(ref pb, ref children) => {
                    try!(writeln!(f, "{}{}",
                                  rep(' ', 2 * depth), pb.display()));
                    for c in children {
                        try!(fmt(f, c, depth + 1));
                    }
                    Ok(())
                }
            }
        }
        fmt(f, self, 0)
    }
}

enum WalkEvent {
    Dir(DirEntry),
    File(DirEntry),
    Exit,
}

struct WalkEventIter {
    depth: usize,
    it: WalkDirIter,
    next: Option<Result<DirEntry, WalkDirError>>,
}

impl<P: AsRef<Path>> From<WalkDir<P>> for WalkEventIter {
    fn from(it: WalkDir<P>) -> WalkEventIter {
        WalkEventIter { depth: 0, it: it.into_iter(), next: None }
    }
}

impl Iterator for WalkEventIter {
    type Item = io::Result<WalkEvent>;

    fn next(&mut self) -> Option<io::Result<WalkEvent>> {
        let dent = self.next.take().or_else(|| self.it.next());
        if self.it.depth() < self.depth {
            self.depth -= 1;
            self.next = dent;
            return Some(Ok(WalkEvent::Exit));
        }
        match dent {
            None => None,
            Some(Err(err)) => Some(Err(From::from(err))),
            Some(Ok(dent)) => {
                match dent.file_type() {
                    Err(err) => Some(Err(err)),
                    Ok(ty) => {
                        if ty.is_dir() {
                            self.depth += 1;
                            Some(Ok(WalkEvent::Dir(dent)))
                        } else {
                            Some(Ok(WalkEvent::File(dent)))
                        }
                    }
                }
            }
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn join(&self, path: &str) -> PathBuf {
        (&*self.0).join(path)
    }

    fn path<'a>(&'a self) -> &'a Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn tmpdir() -> TempDir {
    let p = env::temp_dir();
    let mut r = rand::thread_rng();
    let ret = p.join(&format!("rust-{}", r.next_u32()));
    fs::create_dir(&ret).unwrap();
    TempDir(ret)
}

fn dir_setup_with<F>(t: &Tree, f: F) -> (TempDir, Tree)
        where F: FnOnce(WalkDir<&Path>) -> WalkDir<&Path> {
    let tmp = tmpdir();
    t.create_in(tmp.path()).unwrap();
    let got = Tree::from_walk_with(tmp.path(), f).unwrap();
    (tmp, got.unwrap_singleton())
}

fn dir_setup(t: &Tree) -> (TempDir, Tree) {
    dir_setup_with(t, |wd| wd)
}

fn pb<P: AsRef<Path>>(p: P) -> PathBuf { p.as_ref().to_path_buf() }
fn td<P: AsRef<Path>>(p: P, cs: Vec<Tree>) -> Tree {
    Tree::Dir(pb(p), cs)
}
fn tf<P: AsRef<Path>>(p: P) -> Tree {
    Tree::File(pb(p))
}
fn tl<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> Tree {
    Tree::Symlink(pb(src), pb(dst))
}

#[cfg(unix)]
fn soft_link<P: AsRef<Path>, Q: AsRef<Path>>(
    src: P,
    dst: Q,
) -> io::Result<()> {
    use std::os::unix::fs::symlink;
    symlink(src, dst)
}

#[cfg(windows)]
fn soft_link<P: AsRef<Path>, Q: AsRef<Path>>(
    _src: P,
    _dst: Q,
) -> io::Result<()> {
    unimplemented!()
}

macro_rules! assert_tree_eq {
    ($e1:expr, $e2:expr) => {
        assert_eq!($e1.canonical(), $e2.canonical());
    }
}

#[test]
fn walk_dir_1() {
    let exp = td("foo", vec![]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_2() {
    let exp = tf("foo");
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_3() {
    let exp = td("foo", vec![tf("bar")]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_4() {
    let exp = td("foo", vec![tf("foo"), tf("bar"), tf("baz")]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_5() {
    let exp = td("foo", vec![td("bar", vec![])]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_6() {
    let exp = td("foo", vec![
        td("bar", vec![
           tf("baz"), td("bat", vec![]),
        ]),
    ]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
fn walk_dir_7() {
    let exp = td("foo", vec![
        td("bar", vec![
           tf("baz"), td("bat", vec![]),
        ]),
        td("a", vec![tf("b"), tf("c"), tf("d")]),
    ]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_1() {
    let exp = td("foo", vec![tf("bar"), tl("bar", "baz")]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_2() {
    let exp = td("foo", vec![
        td("a", vec![tf("a1"), tf("a2")]),
        tl("a", "alink"),
    ]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_detect_no_follow_no_loop() {
    let exp = td("foo", vec![
        td("a", vec![tf("a1"), tf("a2")]),
        td("b", vec![tl("../a", "alink")]),
    ]);
    let (_tmp, got) = dir_setup(&exp);
    assert_tree_eq!(exp, got);
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_follow_dir() {
    let actual = td("foo", vec![
        td("a", vec![tf("a1"), tf("a2")]),
        td("b", vec![tl("../a", "alink")]),
    ]);
    let followed = td("foo", vec![
        td("a", vec![tf("a1"), tf("a2")]),
        td("b", vec![td("alink", vec![tf("a1"), tf("a2")])]),
    ]);
    let (_tmp, got) = dir_setup_with(&actual, |wd| wd.follow_links(true));
    assert_tree_eq!(followed, got);
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_detect_loop() {
    let actual = td("foo", vec![
        td("a", vec![tl("../b", "blink"), tf("a1"), tf("a2")]),
        td("b", vec![tl("../a", "alink")]),
    ]);
    let tmp = tmpdir();
    actual.create_in(tmp.path()).unwrap();
    let got = WalkDir::new(tmp.path())
                      .follow_links(true)
                      .into_iter()
                      .collect::<Result<Vec<_>, _>>();
    match got {
        Ok(x) => panic!("expected loop error, got no error: {:?}", x),
        Err(WalkDirError::Io { .. }) => {
            panic!("expected loop error, got generic IO error");
        }
        Err(WalkDirError::Loop { .. }) => {}
    }
}

#[test]
#[cfg(unix)]
fn walk_dir_sym_infinite() {
    let actual = tl("a", "a");
    let tmp = tmpdir();
    actual.create_in(tmp.path()).unwrap();
    let got = WalkDir::new(tmp.path())
                      .follow_links(true)
                      .into_iter()
                      .collect::<Result<Vec<_>, _>>();
    match got {
        Ok(x) => panic!("expected IO error, got no error: {:?}", x),
        Err(WalkDirError::Loop { .. }) => {
            panic!("expected IO error, but got loop error");
        }
        Err(WalkDirError::Io { .. }) => {}
    }
}

#[test]
fn qc_roundtrip() {
    fn p(exp: Tree) -> bool {
        let (_tmp, got) = dir_setup(&exp);
        exp.canonical() == got.canonical()
    }
    QuickCheck::new()
               .gen(StdGen::new(rand::thread_rng(), 15))
               .tests(1_000)
               .max_tests(10_000)
               .quickcheck(p as fn(Tree) -> bool);
}