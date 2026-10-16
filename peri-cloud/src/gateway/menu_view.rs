//! A complete menu snapshot, paged without dropping names or descriptions.
//! Reading a snapshot never refreshes or consumes its action tokens.
use super::{GatewayError, GatewayResult, MenuCard};
use uuid::Uuid;

const PAGE_CHARS: usize = 1200;

pub(crate) fn document(card: &MenuCard) -> String {
    let mut lines = vec![card.title.clone()];
    if !card.buttons.is_empty() {
        lines.push("完整选项（编号对应按钮）：".into());
        for (index, button) in card.buttons.iter().enumerate() {
            // Preserve the full label but keep embedded newlines from impersonating
            // another numbered choice. This is presentation, never action identity.
            let label: String = button
                .label
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            lines.push(format!("/选 {} · {label}", index + 1));
        }
        lines.push("点击按钮，或发送 /选 编号；编号操作当前最新菜单。".into());
    }
    lines.push(String::new());
    lines.push(card.text.clone());
    lines.join("\n")
}

pub(crate) struct MenuPage {
    pub text: String,
    pub page: usize,
    pub pages: usize,
    pub snapshot: Uuid,
}

impl MenuPage {
    pub fn render(&self) -> String {
        if self.pages == 1 {
            return self.text.clone();
        }
        let mut text = format!("菜单全文 {}/{} 页\n{}", self.page, self.pages, self.text);
        if self.page < self.pages {
            text.push_str(&format!(
                "\n\n继续查看：/菜单详情 {} {}",
                self.snapshot,
                self.page + 1
            ));
        }
        if self.page > 1 {
            text.push_str(&format!(
                "\n上一页：/菜单详情 {} {}",
                self.snapshot,
                self.page - 1
            ));
        }
        text
    }
}

pub(crate) fn page(card: &MenuCard, snapshot: Uuid, page: usize) -> GatewayResult<MenuPage> {
    let document = document(card);
    let count = document.chars().count();
    let pages = count.div_ceil(PAGE_CHARS).max(1);
    if page == 0 || page > pages {
        return Err(GatewayError::Invalid);
    }
    Ok(MenuPage {
        text: document
            .chars()
            .skip((page - 1) * PAGE_CHARS)
            .take(PAGE_CHARS)
            .collect(),
        page,
        pages,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::MenuButton;

    #[test]
    fn unicode_pages_reconstruct_every_full_name_and_description() {
        let card = MenuCard {
            title: "目录选择".into(),
            text: format!("{}结尾说明", "完整正文🦀".repeat(900)),
            buttons: vec![MenuButton {
                label: format!("{}甲\n换行", "共同前缀".repeat(80)),
                action_id: Uuid::new_v4(),
            }],
            question_request: None,
        };
        let id = Uuid::new_v4();
        let first = page(&card, id, 1).unwrap();
        assert!(first.render().contains(&format!("/菜单详情 {id} 2")));
        let restored: String = (1..=first.pages)
            .map(|n| page(&card, id, n).unwrap().text)
            .collect();
        assert_eq!(restored, document(&card));
        assert!(restored.contains("甲 换行"));
        assert!(restored.ends_with("结尾说明"));
        assert!(!restored.contains('…'));
        assert!(page(&card, id, 0).is_err());
        assert!(page(&card, id, usize::MAX).is_err());
    }
}
