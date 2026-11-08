/*
 * Copyright (C) 2026 IchthysOS
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.ichthys.abyss;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.SystemProperties;
import android.util.Log;

/**
 * Back from Abyss. Leaving Abyss restarts the framework (not the kernel), which sends
 * LOCKED_BOOT_COMPLETED and BOOT_COMPLETED again, so this covers both the normal "leave" path
 * and a real reboot while Abyss was active.
 * <ul>
 * <li>{@link GoDark#restore}: radios and double-tap wake as they were before entering.</li>
 * <li>{@link WifiHandoff#clear}: the Wi-Fi hand-over (it holds passphrases) is deleted.</li>
 * <li>{@link MobileHandoff#clear}: the mobile-data hand-over (an APN may have a password) too.</li>
 * <li>A stale persist.abyss.after_leave (an older abyssd wrote "light" to open Abyss Light) is
 *     cleared. Abyss Light is retired (owner, 2026-11-07): nothing is ever opened from here.</li>
 * </ul>
 * persist.abyss.after_leave is abyss_after_leave_prop; system_app may get/set it
 * (abyss_entry.te).
 */
public final class BootReceiver extends BroadcastReceiver {
    private static final String TAG = "AbyssEntry";
    static final String PROP_AFTER_LEAVE = "persist.abyss.after_leave";

    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent.getAction();
        if (!Intent.ACTION_LOCKED_BOOT_COMPLETED.equals(action)
                && !Intent.ACTION_BOOT_COMPLETED.equals(action)) {
            return;
        }
        GoDark.restore(context);
        WifiHandoff.clear();
        MobileHandoff.clear();
        if (!SystemProperties.get(PROP_AFTER_LEAVE, "").isEmpty()) {
            try {
                SystemProperties.set(PROP_AFTER_LEAVE, "");
            } catch (RuntimeException e) {
                Log.w(TAG, "could not clear " + PROP_AFTER_LEAVE, e);
            }
        }
    }
}
