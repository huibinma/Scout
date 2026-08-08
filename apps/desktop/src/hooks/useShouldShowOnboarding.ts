import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';

export interface OnboardingState {
  macos_fda_shown: boolean;
  windows_indexing_shown: boolean;
  model_download_shown: boolean;  // BETA-31
}

export type OnboardingType = 'none' | 'macos' | 'windows' | 'loading';

export function useShouldShowOnboarding() {
  const [shouldShow, setShouldShow] = useState<OnboardingType>('loading');

  useEffect(() => {
    async function check() {
      try {
        const state = await invoke<OnboardingState>('get_onboarding_state');

        // macOS：FDA onboarding 未完成 → 走 macos 路径
        const macStatus = await invoke<string>('check_macos_full_disk_access');
        if (macStatus !== 'NotApplicable') {
          if (!state.macos_fda_shown) {
            setShouldShow('macos');
            return;
          }
        }

        // Windows：搜索索引 onboarding 未完成 → 走 windows 路径
        const winStatus = await invoke<string>('check_windows_search_indexed');
        if (winStatus !== 'NotApplicable') {
          if (!state.windows_indexing_shown) {
            setShouldShow('windows');
            return;
          }
        }

        // BETA-31：model_download 未完成时、仍走平台 onboarding 路径
        // （含 Step 2/3 模型下载 + 示例查询）
        if (!state.model_download_shown) {
          if (macStatus !== 'NotApplicable') {
            setShouldShow('macos');
            return;
          }
          if (winStatus !== 'NotApplicable') {
            setShouldShow('windows');
            return;
          }
        }

        setShouldShow('none');
      } catch (e) {
        console.error('Failed to check onboarding state', e);
        setShouldShow('none');
      }
    }
    check();
  }, []);

  return shouldShow;
}
