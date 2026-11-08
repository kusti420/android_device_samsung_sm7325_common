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

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.os.Bundle;

/**
 * One section of Settings › Abyss (Playback &amp; Bluetooth, Internet, ...). What it shows is
 * built by {@link Sections} from the section id in {@link #EXTRA_SECTION}; this activity only
 * hosts the page. Not exported: reached from the control center only.
 */
public final class SectionActivity extends Activity {
    static final String EXTRA_SECTION = "org.ichthys.abyss.SECTION";

    private Page mPage;

    static Intent intent(Context c, String id) {
        return new Intent(c, SectionActivity.class).putExtra(EXTRA_SECTION, id);
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Sections.Section s = Sections.find(String.valueOf(
                getIntent().getStringExtra(EXTRA_SECTION)));
        if (s == null || s.build == null) {
            finish();
            return;
        }
        mPage = new Page(this, getString(s.title), true);
        s.build.accept(mPage);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (mPage != null) {
            mPage.refresh();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (mPage != null) {
            mPage.onActivityResult(requestCode, resultCode, data);
        }
    }
}
