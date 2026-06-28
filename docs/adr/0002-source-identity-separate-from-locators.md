# Source Identity Separate From Locators

qgh source identity is a qgh URI built from GitHub `node_id`, for example `qgh://github.com/issue/<percent-encoded-node_id>` or `qgh://github.com/issue-comment/<percent-encoded-node_id>`.

Canonical URL, title, issue number, comment URL, and REST numeric `id` are locators or secondary identifiers, not primary identity. Those locators can change through transfer, edit, or delete, so treating them as identity would create wrong-source retrieval and stale citation failures.
