/// A row being dragged to another place in its list, or dropped there and not saved yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Reorder {
    pub list: List,
    pub id: String,
    pub to: usize,
    /// How far the row is drawn from the place it would take.
    pub offset: f32,
    pub dropped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum List {
    Providers,
    Accounts(String),
}

impl Reorder {
    pub fn arrange<'a, T>(&self, items: &'a [T], id: impl Fn(&T) -> &str) -> Vec<&'a T> {
        let mut arranged: Vec<&T> = items.iter().collect();
        if let Some(from) = items.iter().position(|item| id(item) == self.id) {
            let item = arranged.remove(from);
            arranged.insert(self.to.min(arranged.len()), item);
        }
        arranged
    }
}

/// The place a row dragged by `travel` takes, and how far it is drawn from there.
pub fn place(heights: &[f32], gap: f32, from: usize, travel: f32) -> (usize, f32) {
    let passed: Vec<usize> = if travel > 0.0 { (from + 1..heights.len()).collect() } else { (0..from).rev().collect() };
    let room: f32 = passed.iter().map(|index| heights[*index] + gap).sum();
    let distance = travel.abs().min(room);
    let (mut to, mut shift) = (from, 0.0);
    for index in passed {
        let step = heights[index] + gap;
        if distance < shift + step / 2.0 {
            break;
        }
        shift += step;
        to = index;
    }
    (to, (distance - shift) * travel.signum())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEIGHTS: [f32; 3] = [100.0, 40.0, 60.0];

    #[test]
    fn a_row_takes_a_place_once_it_is_dragged_halfway_past_its_neighbor() {
        assert_eq!(place(&HEIGHTS, 2.0, 0, 20.0), (0, 20.0));
        assert_eq!(place(&HEIGHTS, 2.0, 0, 21.0), (1, -21.0));
        assert_eq!(place(&HEIGHTS, 2.0, 0, 80.0), (2, -24.0));
        assert_eq!(place(&HEIGHTS, 2.0, 2, -21.0), (1, 21.0));
        assert_eq!(place(&HEIGHTS, 2.0, 2, -100.0), (0, 44.0));
    }

    #[test]
    fn a_row_stays_within_its_list() {
        assert_eq!(place(&HEIGHTS, 2.0, 0, 900.0), (2, 0.0));
        assert_eq!(place(&HEIGHTS, 2.0, 0, -50.0), (0, 0.0));
        assert_eq!(place(&HEIGHTS, 2.0, 2, -900.0), (0, 0.0));
    }

    #[test]
    fn a_removed_row_leaves_the_others_in_place() {
        let reorder = |id: &str, to| Reorder { list: List::Providers, id: id.into(), to, offset: 0.0, dropped: false };
        let items = ["a", "b", "c"];
        assert_eq!(reorder("a", 2).arrange(&items, |item| item), [&"b", &"c", &"a"]);
        assert_eq!(reorder("gone", 0).arrange(&items, |item| item), [&"a", &"b", &"c"]);
    }
}
