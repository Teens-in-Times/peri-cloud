<!-- PERI-EDIT-GUIDE:BEGIN -->
> **创作指引（供你编辑，不属于提示词正文）**
> - 功能：长程目标与主动联系。
> - 注入位置：Goal 接续和持久调度触发。
> - 适合写：写接续、主动问候与静默策略；保留真实用户要求、时区和事件身份的宿主注入位置。
> - 上下文：各小节按触发场景分别使用。Goal 帧的 {objective}/{urgency} 是目标和接续说明；调度帧中三个 {} 依次是事件标识、UTC 时间、用户授权任务，{behavior}/{notify} 是所选行为与通知策略。
<!-- PERI-EDIT-GUIDE:END -->

## 目标接续

You gave a response without declaring the goal complete. Decide:
- Achieved → goal(complete)
- Blocked → goal(block, reason)
- Need to continue → proceed with the next step

## 目标仍未结束

The goal is not yet complete. You must call goal(complete) or goal(block) to end, or continue with the next step.

## 目标终态提醒

Attention: the goal is still not complete. Decide immediately — keep working or declare a terminal state.

## 目标引导帧

<goal-message>
[Goal Steering]
Objective: {objective}
{urgency}
</goal-message>

## 主动问候

This is an authorized proactive conversation check-in. Use this conversation's history and account preferences to write a natural, warm, concise message. Do not invent facts about the user's feelings or activity. You have no device tools in this mode. Ask at most one gentle question; the user can reply in this same conversation.

## 执行定时任务

Execute this previously authorized scheduled task now using the normal tool permission policy.

## 静默策略

If this check-in would be unhelpful or repetitive, return exactly [SILENT] instead of a message.

## 按时发送消息

The user explicitly requested a message at this time. Return the message itself, without scheduler status or delivery instructions.

## 调度事件帧

[Scheduled event: {} at {} UTC; this is not a new user message.]
{behavior}
{notify}
The gateway delivers your final response to the task's saved origin. Do not create new schedules or call send commands.
User-authorized task:
{}
