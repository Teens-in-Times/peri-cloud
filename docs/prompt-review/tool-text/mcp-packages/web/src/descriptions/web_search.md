<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具或中间件说明。
> - 作用位置：受实际工具与中间件装配开关控制。
> - 应当写：写使用时机、输入输出和限制；保持与实际能力一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

Search the web using a search engine.

Usage:
- Provide a search query to find relevant web pages
- Returns results as a numbered Markdown list with titles, URLs, and text snippets
- Each result's text is truncated to 500 characters
- No API key required

IMPORTANT:
- Results may be irrelevant or low quality — always verify information before using it
- If results don't contain the information you need, do NOT fabricate or guess values
- Consider using WebFetch to directly access a specific URL for accurate information

Parameters:
- query (required): Search keywords
- num_results (optional): Number of results, default 10, max 20
