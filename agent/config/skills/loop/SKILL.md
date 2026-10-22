---
name: loop
description: Run a prompt or Skill again and again in this Chat, on a fixed interval (`/loop 5m check the deploy`, `/loop check the deploy every 2 hours`) or self-paced when no interval is given. Without a prompt it runs the project's loop.md or the built-in maintenance instructions. Use it when the user wants a recurring task, to poll for status or to keep running something repeatedly; not for a one-off task.
---

# /loop

The user ran /loop to repeat work in this Chat's Loop. The host has already parsed the input: the section "This /loop" at the end gives the form, what to run now and the scheduled prompt. Follow the section below for that form.

Scheduled tasks belong to the root Agent of a local Chat. If CronCreate or ScheduleWakeup is not among your tools, tell the user that /loop is not available here and stop without running anything.

When what you run is a slash command naming a Skill (for example `/review-pr 1234`), invoke that Skill with the Skill tool; otherwise act on it directly. Always pass the scheduled prompt to CronCreate or ScheduleWakeup exactly as "This /loop" gives it, slash command included.

## Fixed interval

1. Convert the interval to a 5-field cron expression in local time. Seconds round up to whole minutes; the shortest interval is one minute.

   | Interval | Cron expression |
   | --- | --- |
   | `Nm`, N up to 59 | `*/N * * * *` |
   | `Nm`, N of 60 or more | `0 */H * * *` with H = N / 60 |
   | `Nh`, N up to 23 | `0 */N * * *` |
   | `Nd` | `0 0 */N * *` (midnight local time) |

   When the interval does not divide its unit evenly (`7m` leaves an uneven gap at the top of the hour; `90m` is no whole number of hours), use the nearest interval that does and tell the user which one you picked.
2. Call CronCreate with `cron` (the expression), `prompt` (the scheduled prompt) and `recurring: true`.
3. Confirm briefly: what runs, the cadence and next run the CronCreate result reports (the host states the interval it actually uses), the task id, that the task ends by itself after 7 days, and that the user can cancel it sooner by asking you (CronDelete) or with `x` on its row in the Monitor.
4. Then run it once now; do not wait for the first scheduled run.

A run that comes due while the Loop is busy waits until the current turn ends. Esc does not stop a fixed-interval task.

## Self-paced

Without an interval you choose when the next run happens.

1. Run it now.
2. At the end of every run, decide whether the loop continues. Continuing is a choice each run makes, not a default.
   - To continue, call ScheduleWakeup with `delaySeconds` (60 to 3600), `reason` (one short sentence on why you picked that delay) and `prompt` (the scheduled prompt, which brings the next run back into this Skill).
   - To end the loop, because the task is done, further runs cannot make progress or the user asked you to stop, call ScheduleWakeup with `stop: true` and no other fields.
   If a run ends with neither, the host wakes the Loop once more after about 20 minutes and ends the loop when that run does not reschedule either.
3. Pick the delay from what you observed this run:
   - Work the host tracks, such as a background Shell command, a Subagent or a Tick this Loop subscribes to, wakes the Loop by itself when it changes. Do not poll it with short wakeups; schedule a long fallback (1200 seconds or more) in case it never reports.
   - External state the host cannot see, such as a CI run, a deploy or a remote queue: match how fast it changes, for example 60 to 270 seconds while a build is finishing.
   - Nothing pending: 1200 to 1800 seconds.
4. End every run with a short visible update: what you did, and the delay you picked and why, or that the loop ended. On the first run, also say that the loop is self-paced and that Esc stops it while it waits.

## No prompt

"This /loop" carries the loop instructions to run: the project's `.tickloop/loop.md`, else `loop.md` in the Tickloop config root, else the built-in maintenance instructions. The scheduled prompt makes the host read them again at every run, so edits to loop.md take effect on the next run.

- With an interval, follow "Fixed interval" and also tell the user where the instructions come from.
- Without an interval, follow "Self-paced".
