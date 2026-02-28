// pm_mod.hpp — Mod hot-reload: watch .so files and swap at runtime
//
// Mods are shared libraries (.so) compiled against pm headers.
// They must export two C symbols:
//
//   extern "C" void pm_mod_load(Pm& pm);
//     Called after dlopen. Register tasks, access pools/state here.
//
//   extern "C" void pm_mod_unload(Pm& pm);
//     Called before dlclose. Must task_stop ALL tasks and remove
//     any callbacks registered with other systems (debug, net, etc).
//
// Usage:
//   ModLoader mods;
//   mods.watch(exe_dir() + "mods/my_mod.so");
//   mods.load_all(pm);
//   pm.task_add("mods/poll", Phase::INPUT - 5.f, [&mods](Pm& pm) {
//       mods.poll(pm);
//   });
//   pm.loop_run();
//   mods.unload_all(pm);
//
// Hot-reload: rebuild the .so while the game is running.
// On the next poll(), the loader detects the mtime change and reloads.

#pragma once

#include "pm_core.hpp"
#include <dlfcn.h>
#include <sys/stat.h>
#include <cstdio>
#include <string>
#include <vector>

namespace pm {

struct ModLoader
{
	void watch(std::string path)
	{
		Entry e;
		e.name = basename_noext(path);
		e.path = std::move(path);
		entries_.push_back(std::move(e));
	}

	void load_all(Pm &pm)
	{
		for (auto &e : entries_)
		{
			time_t mt = file_mtime(e.path);
			if (mt != 0)
				do_load(pm, e);
		}
	}

	void poll(Pm &pm)
	{
		for (auto &e : entries_)
		{
			time_t mt = file_mtime(e.path);
			if (mt == 0)
				continue;
			if (mt != e.mtime)
			{
				if (e.handle)
					do_unload(pm, e);
				do_load(pm, e);
			}
		}
	}

	void unload_all(Pm &pm)
	{
		for (auto &e : entries_)
			if (e.handle)
				do_unload(pm, e);
	}

private:
	struct Entry
	{
		std::string path;
		std::string name;
		void *handle = nullptr;
		time_t mtime = 0;
	};
	std::vector<Entry> entries_;

	static time_t file_mtime(const std::string &path)
	{
		struct stat st;
		return (stat(path.c_str(), &st) == 0) ? st.st_mtime : 0;
	}

	static std::string basename_noext(const std::string &path)
	{
		auto slash = path.rfind('/');
		size_t start = (slash == std::string::npos) ? 0 : slash + 1;
		auto dot = path.rfind('.');
		size_t end = (dot == std::string::npos || dot < start) ? path.size() : dot;
		return path.substr(start, end - start);
	}

	using ModFn = void (*)(Pm &);

	void do_load(Pm &pm, Entry &e)
	{
		void *handle = dlopen(e.path.c_str(), RTLD_NOW | RTLD_LOCAL);
		if (!handle)
		{
			fprintf(stderr, "[mod] load failed '%s': %s\n", e.name.c_str(), dlerror());
			return;
		}

		auto *load_fn = reinterpret_cast<ModFn>(dlsym(handle, "pm_mod_load"));
		if (!load_fn)
		{
			fprintf(stderr, "[mod] '%s' missing pm_mod_load: %s\n", e.name.c_str(), dlerror());
			dlclose(handle);
			return;
		}

		e.handle = handle;
		e.mtime = file_mtime(e.path);
		load_fn(pm);
		printf("[mod] loaded: %s\n", e.name.c_str());
	}

	void do_unload(Pm &pm, Entry &e)
	{
		auto *unload_fn = reinterpret_cast<ModFn>(dlsym(e.handle, "pm_mod_unload"));
		if (unload_fn)
			unload_fn(pm);
		else
			fprintf(stderr, "[mod] '%s' missing pm_mod_unload — skipping cleanup\n", e.name.c_str());

		dlclose(e.handle);
		e.handle = nullptr;
		printf("[mod] unloaded: %s\n", e.name.c_str());
	}
};

} // namespace pm