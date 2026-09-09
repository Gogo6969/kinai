<script lang="ts">
  import {
    Globe,
    Calculator,
    Clock,
    MessagesSquare,
    CheckCheck,
    Loader2,
    XCircle,
    AlarmClockPlus,
    AlarmClockOff,
    CalendarDays,
  } from '@lucide/svelte';

  let { name, ok }: { name: string; ok?: boolean } = $props();

  const Icon = $derived(
    name === 'web_search'
      ? Globe
      : name === 'x_search'
        ? MessagesSquare
        : name === 'calculator'
          ? Calculator
          : name === 'datetime'
            ? Clock
            : name === 'set_reminder'
              ? AlarmClockPlus
              : name === 'list_reminders'
                ? CalendarDays
                : name === 'cancel_reminder'
                  ? AlarmClockOff
                  : Globe
  );

  const label = $derived(
    name === 'web_search'
      ? 'Searching the web'
      : name === 'x_search'
        ? 'Searching social posts'
        : name === 'calculator'
          ? 'Calculating'
          : name === 'datetime'
            ? 'Checking the date'
            : name === 'set_reminder'
              ? 'Setting a reminder'
              : name === 'list_reminders'
                ? 'Checking reminders'
                : name === 'cancel_reminder'
                  ? 'Cancelling a reminder'
                  : name
  );

  const StatusIcon = $derived(ok === undefined ? Loader2 : ok ? CheckCheck : XCircle);
</script>

<div class="kin-badge">
  <Icon size={12} class="opacity-80" />
  <span>{label}</span>
  <StatusIcon size={12} class={ok === undefined ? 'animate-spin' : ''} />
</div>
