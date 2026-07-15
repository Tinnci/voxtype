cmake_minimum_required(VERSION 3.20)
project(voxtype-fcitx5 VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Fcitx5Core 5.1.7 REQUIRED)
include("${Fcitx5Utils_DIR}/Fcitx5Macros.cmake")

add_fcitx5_addon(voxtypebridge voxtypebridge.cpp)
target_link_libraries(voxtypebridge Fcitx5::Core)
target_compile_options(voxtypebridge PRIVATE -Wall -Wextra -Wpedantic -Werror)

install(TARGETS voxtypebridge DESTINATION lib/fcitx5)
install(FILES voxtypebridge.conf DESTINATION share/fcitx5/addon)
