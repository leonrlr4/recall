import { describe, expect, it } from 'vitest';
import { shiftAfterDelete } from './transcript';

describe('transcript', () => {
  it('shift_after_delete_moves_later_lines_down', () => {
    expect(shiftAfterDelete(5, 2)).toBe(4);
    expect(shiftAfterDelete(1, 2)).toBe(1);
    expect(shiftAfterDelete(2, 2)).toBe(null);
    expect(shiftAfterDelete(null, 2)).toBe(null);
    expect(shiftAfterDelete(0, 0)).toBe(null);
  });
});
