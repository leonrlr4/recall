// Reactive holder for the UI locale. Kept in a `.svelte.ts` module so that
// templates calling `t()` re-render when `setLocale` changes it.
export const lang = $state<{ current: 'zh-TW' | 'en' }>({ current: 'en' });
